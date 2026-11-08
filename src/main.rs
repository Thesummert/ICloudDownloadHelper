mod auth;
mod download;
mod filter;
mod photos;
mod srp;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use photos::Album;
use std::{collections::HashSet, path::PathBuf, time::Duration};

#[derive(Clone, Copy, ValueEnum)]
enum Domain {
    Com,
    Cn,
}

#[derive(Parser)]
#[command(
    name = "icloud-dl",
    version,
    about = "跨平台 iCloud 照片下载：独立登录、相册选择、原图与并发下载"
)]
struct Cli {
    #[arg(
        short = 'u',
        long,
        env = "ICLOUD_USERNAME",
        global = true,
        help = "Apple 账号邮箱/手机号；留空则交互输入"
    )]
    username: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value = "com",
        global = true,
        help = "中国大陆云上贵州账号请选择 cn"
    )]
    domain: Domain,
    #[arg(long, global = true, help = "会话文件保存目录；不保存密码")]
    session_dir: Option<PathBuf>,
    #[arg(long, global = true, help = "忽略已保存的会话并重新登录")]
    fresh_login: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "登录 Apple 账号并完成双重认证")]
    Login,
    #[command(about = "列出个人照片库中的相册和相册 ID")]
    Albums,
    #[command(about = "将指定相册中的原始照片和视频下载到本地")]
    Download(DownloadArgs),
}

#[derive(Args)]
struct DownloadArgs {
    #[arg(short = 'd', long, help = "下载目录（支持外置硬盘目录）")]
    directory: PathBuf,
    #[arg(
        short = 'a',
        long,
        conflicts_with = "all",
        help = "相册精确名称或 ID；可重复传入；留空则交互选择"
    )]
    album: Vec<String>,
    #[arg(
        long,
        conflicts_with = "album",
        help = "下载整个个人照片库（不含隐藏和最近删除）"
    )]
    all: bool,
    #[arg(short='j',long,default_value_t=8,value_parser=clap::value_parser!(u16).range(1..=64),help="并发下载数，1–64")]
    concurrency: u16,
    #[arg(long,default_value_t=3,value_parser=clap::value_parser!(u32).range(0..=10),help="每个文件失败后的重试次数")]
    retries: u32,
    #[arg(long, help = "同时下载 Live Photo 的原始 MOV 视频")]
    live: bool,
    #[arg(long, help = "仅列出计划下载的文件，不创建下载目录或下载媒体")]
    dry_run: bool,
    #[arg(long, help = "拍摄日期起始日（UTC，YYYY-MM-DD，含当天）")]
    from: Option<String>,
    #[arg(long, help = "拍摄日期结束日（UTC，YYYY-MM-DD，含当天）")]
    to: Option<String>,
    #[arg(long, help = "本次最多选择的文件数，包含已存在和失败的文件")]
    limit: Option<String>,
}

#[derive(Default)]
struct Counts {
    downloaded: usize,
    skipped: usize,
    failed: usize,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    tokio::select! {
        result = run(cli) => match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => { eprintln!("错误：{e:#}"); std::process::ExitCode::FAILURE }
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n已停止。重新运行相同命令可校验已完成文件并继续临时文件。");
            std::process::ExitCode::from(130)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let mut filter = match &cli.command {
        Command::Download(args) => Some(filter::DownloadFilter::new(
            args.from.as_deref().unwrap_or(""),
            args.to.as_deref().unwrap_or(""),
            args.limit.as_deref().unwrap_or(""),
        )?),
        _ => None,
    };
    let username = match cli.username {
        Some(u) => u,
        None => auth::prompt("Apple 账号邮箱/手机号")?,
    };
    ensure!(!username.is_empty(), "Apple 账号不能为空");
    let domain = match cli.domain {
        Domain::Com => "com",
        Domain::Cn => "cn",
    };
    let session_dir = match cli.session_dir {
        Some(p) => p,
        None => directories::ProjectDirs::from("", "", "icloud-download-helper")
            .context("无法定位配置目录，请传入 --session-dir")?
            .config_dir()
            .join("sessions"),
    };
    let mut session = auth::Session::new(&username, domain, &session_dir)?;
    eprintln!("正在连接 iCloud（{domain}）…");
    let account = session.login(cli.fresh_login).await?;
    if matches!(cli.command, Command::Login) {
        println!("登录成功。会话已保存到 {}", session_dir.display());
        return Ok(());
    }
    let photos = photos::Photos::new(&session, &account)?;
    photos.check_index().await?;
    if matches!(cli.command, Command::Albums) {
        let albums = photos.albums().await?;
        println!("相册名称\t相册 ID");
        for a in &albums {
            println!("{}\t{}", a.name, a.id);
        }
        println!("共 {} 个相册；--all 可下载整个个人照片库。", albums.len());
        return Ok(());
    }
    let Command::Download(args) = cli.command else {
        unreachable!()
    };
    let selected = if args.all {
        vec![None]
    } else {
        let albums = photos.albums().await?;
        select_albums(albums, &args.album)?
            .into_iter()
            .map(Some)
            .collect()
    };
    // Lock the output tree across processes. The OS releases it after Ctrl+C/crash.
    let _lock = if args.dry_run {
        None
    } else {
        std::fs::create_dir_all(&args.directory).context("无法创建下载目录")?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(args.directory.join(".icloud-dl.lock"))?;
        file.try_lock().context("另一个任务正在使用此下载目录")?;
        Some(file)
    };
    // Use a separate client: signed media URLs must not receive account cookies.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()?;
    let mut counts = Counts::default();
    'albums: for album in selected {
        let (label, dir) = match &album {
            Some(a) => (
                a.name.clone(),
                args.directory.join(format!(
                    "{}_{}",
                    download::safe_name(&a.name),
                    download::identity(&a.id)
                )),
            ),
            None => ("整个照片库".into(), args.directory.join("AllPhotos")),
        };
        eprintln!("相册：{label} → {}", dir.display());
        let mut offset = 0u64;
        let mut seen = HashSet::new();
        let bar = ProgressBar::new_spinner();
        bar.set_style(ProgressStyle::with_template(
            "{spinner:.green} {msg} [{elapsed_precise}]",
        )?);
        bar.enable_steady_tick(Duration::from_millis(100));
        loop {
            bar.set_message(format!("读取 {label}（位置 {offset}）"));
            let page = photos.page(album.as_ref(), offset, args.live).await?;
            if page.empty {
                break;
            }
            offset += page.masters as u64;
            let media: Vec<_> = page
                .media
                .into_iter()
                .filter(|m| seen.insert(m.id.clone()))
                .collect();
            ensure!(!media.is_empty(), "Apple 分页没有前进，停止以避免重复下载");
            let filter = filter.as_mut().expect("download filter exists");
            let mut selected_media = Vec::new();
            for item in media {
                if filter.full() {
                    break;
                }
                if filter.select(&item)? {
                    selected_media.push(item);
                }
            }
            let media = selected_media;
            let full = filter.full();
            if args.dry_run {
                for m in media {
                    println!(
                        "{}\t{} bytes",
                        download::destination(&dir, &m).display(),
                        m.size
                    );
                }
                if full {
                    break 'albums;
                }
                continue;
            }
            let mut jobs = stream::iter(media.iter().map(|m| {
                let client = &client;
                let dir = &dir;
                async move { (m, download::download(client, dir, m, args.retries).await) }
            }))
            .buffer_unordered(args.concurrency as usize);
            while let Some((m, result)) = jobs.next().await {
                match result {
                    Ok(download::Outcome::Downloaded) => counts.downloaded += 1,
                    Ok(download::Outcome::Skipped) => counts.skipped += 1,
                    Err(e) => {
                        counts.failed += 1;
                        bar.println(format!("失败 {}：{e:#}", m.filename));
                    }
                }
                bar.set_message(format!(
                    "{label}：完成 {}，跳过 {}，失败 {}",
                    counts.downloaded, counts.skipped, counts.failed
                ));
            }
            if full {
                bar.finish_and_clear();
                break 'albums;
            }
        }
        bar.finish_and_clear();
    }
    if args.dry_run {
        println!("预览完成，未下载媒体。");
    } else {
        println!(
            "完成：下载 {}，已存在 {}，失败 {}。",
            counts.downloaded, counts.skipped, counts.failed
        );
    }
    ensure!(
        counts.failed == 0,
        "部分文件下载失败，重新运行相同命令可重试"
    );
    Ok(())
}

fn select_albums(albums: Vec<Album>, selectors: &[String]) -> Result<Vec<Album>> {
    if selectors.is_empty() {
        ensure!(
            !albums.is_empty(),
            "未找到相册，请使用 --all 下载个人照片库"
        );
        for (i, a) in albums.iter().enumerate() {
            println!("{}. {}", i + 1, a.name);
        }
        let input = auth::prompt("选择相册序号（用英文逗号分隔）")?;
        let mut selected = Vec::new();
        let mut seen = HashSet::new();
        for index in input.split(',') {
            let n: usize = index.trim().parse().context("相册序号必须是数字")?;
            let album = n
                .checked_sub(1)
                .and_then(|n| albums.get(n))
                .context("相册序号超出范围")?;
            if seen.insert(album.id.clone()) {
                selected.push(album.clone());
            }
        }
        return Ok(selected);
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for s in selectors {
        let by_id = albums.iter().find(|a| &a.id == s);
        let matches: Vec<_> = if let Some(a) = by_id {
            vec![a]
        } else {
            albums.iter().filter(|a| &a.name == s).collect()
        };
        ensure!(
            !matches.is_empty(),
            "未找到相册：{s}，请使用 albums 查看可选名称"
        );
        ensure!(
            matches.len() == 1,
            "多个相册同名：{s}，请使用 albums 输出的相册 ID"
        );
        if seen.insert(matches[0].id.clone()) {
            result.push(matches[0].clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn albums() -> Vec<Album> {
        vec![
            Album {
                id: "1".into(),
                name: "旅行".into(),
            },
            Album {
                id: "2".into(),
                name: "旅行".into(),
            },
        ]
    }
    #[test]
    fn selects_ids_and_rejects_ambiguous_names() {
        assert!(select_albums(albums(), &["旅行".into()]).is_err());
        assert!(select_albums(albums(), &["missing".into()]).is_err());
        assert_eq!(
            select_albums(albums(), &["2".into(), "2".into()])
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn validates_concurrency_and_selection_conflicts() {
        assert!(Cli::try_parse_from(["icloud-dl", "download", "-d", "out", "-j", "0"]).is_err());
        assert!(
            Cli::try_parse_from(["icloud-dl", "download", "-d", "out", "--all", "-a", "旅行"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["icloud-dl", "download", "-d", "out", "--all", "-j", "8"]).is_ok()
        );
    }
}

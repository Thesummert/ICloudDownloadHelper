use crate::photos::Media;
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_RANGE, RANGE},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

#[derive(Serialize, Deserialize)]
struct Receipt {
    size: u64,
    sha256: String,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Downloaded,
    Skipped,
}

pub fn safe_name(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "/\\<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut name = name.trim_matches([' ', '.']).to_owned();
    if name.is_empty() {
        name = "unnamed".into();
    }
    // Leave room for the identity prefix and suffixes on common filesystems.
    while name.len() > 170 {
        name.pop();
    }
    name
}

pub fn identity(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..32].to_owned()
}

pub fn destination(directory: &Path, media: &Media) -> PathBuf {
    let key = identity(&format!(
        "{}:{}:{}",
        media.id, media.fingerprint, media.size
    ));
    directory.join(format!("{key}_{}", safe_name(&media.filename)))
}

async fn file_hash(path: &Path) -> Result<String> {
    let mut input = fs::File::open(path).await?;
    let mut buffer = vec![0; 128 * 1024];
    let mut hash = Sha256::new();
    loop {
        let n = input.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub async fn download(
    client: &Client,
    directory: &Path,
    media: &Media,
    retries: u32,
) -> Result<Outcome> {
    download_with_progress(client, directory, media, retries, |_, _| {}).await
}

/// Reports the current file offset (including resumed bytes), which may reset on retry.
pub async fn download_with_progress(
    client: &Client,
    directory: &Path,
    media: &Media,
    retries: u32,
    mut progress: impl FnMut(u64, u64),
) -> Result<Outcome> {
    progress(0, media.size);
    fs::create_dir_all(directory).await?;
    let dest = destination(directory, media);
    let receipt_path = dest.with_file_name(format!(
        "{}.done.json",
        dest.file_name().unwrap().to_string_lossy()
    ));
    if dest.exists() {
        let receipt = fs::read(&receipt_path)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Receipt>(&b).ok());
        if let Some(receipt) = receipt
            && receipt.size == media.size
            && fs::metadata(&dest).await?.len() == media.size
            && file_hash(&dest).await? == receipt.sha256
        {
            progress(media.size, media.size);
            return Ok(Outcome::Skipped);
        }
        anyhow::bail!(
            "目标文件已存在，但缺少有效完成记录或校验失败：{}；请移走此文件后重试",
            dest.display()
        );
    }
    let part = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().unwrap().to_string_lossy()
    ));
    let mut last_error = None;
    for attempt in 0..=retries {
        match transfer(client, media, &part, &mut progress).await {
            Ok(()) => {
                let receipt = Receipt {
                    size: media.size,
                    sha256: file_hash(&part).await?,
                };
                // Persist receipt first: an interruption after rename can still be verified.
                crate::auth::write_private(&receipt_path, &serde_json::to_vec(&receipt)?)?;
                ensure!(
                    !dest.exists(),
                    "目标文件在下载期间被其他程序创建：{}",
                    dest.display()
                );
                fs::rename(&part, &dest).await.context("保存下载文件失败")?;
                return Ok(Outcome::Downloaded);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
        if attempt < retries {
            tokio::time::sleep(Duration::from_secs(1 << attempt.min(5))).await;
        }
    }
    Err(last_error.context("下载失败")?)
}

fn range_matches(header: &str, offset: u64, size: u64) -> bool {
    let Some(value) = header.strip_prefix("bytes ") else {
        return false;
    };
    let Some((interval, total)) = value.split_once('/') else {
        return false;
    };
    let Some((start, end)) = interval.split_once('-') else {
        return false;
    };
    start.parse::<u64>().ok() == Some(offset)
        && total.parse::<u64>().ok() == Some(size)
        && end.parse::<u64>().ok() == size.checked_sub(1)
}

async fn transfer(
    client: &Client,
    media: &Media,
    part: &Path,
    progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    let mut offset = match fs::metadata(part).await {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    if offset == media.size {
        progress(offset, media.size);
        return Ok(());
    }
    if offset > media.size {
        fs::remove_file(part).await?;
        offset = 0;
    }
    let mut request = client.get(&media.url).header("Accept-Encoding", "identity");
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let response = request
        .send()
        .await
        .map_err(|e| e.without_url())
        .context("下载连接失败")?;
    let status = response.status();
    ensure!(
        status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT,
        "下载服务器返回 HTTP {}{}",
        status.as_u16(),
        if matches!(status.as_u16(), 401 | 403) {
            "，地址可能过期，请重新运行以刷新"
        } else {
            ""
        }
    );
    if status == StatusCode::PARTIAL_CONTENT {
        let range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        ensure!(
            range_matches(range, offset, media.size),
            "服务器返回了不匹配的 Content-Range"
        );
    } else {
        offset = 0;
    } // A server may ignore Range; restart instead of appending.
    if let Some(length) = response.content_length() {
        ensure!(
            length == media.size - offset,
            "服务端长度与 iCloud 元数据不符"
        );
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(offset == 0)
        .append(offset > 0)
        .open(part)
        .await?;
    progress(offset, media.size);
    let mut written = offset;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| e.without_url())
            .context("下载中断，已保留临时文件")?;
        ensure!(
            chunk.len() as u64 <= media.size - written,
            "响应超过预期文件大小"
        );
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        progress(written, media.size);
    }
    file.flush().await?;
    ensure!(
        written == media.size,
        "下载不完整：{written}/{} 字节",
        media.size
    );
    file.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn server(
        response: &'static str,
        expected_range: Option<&'static str>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/photo", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if socket.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            if let Some(range) = expected_range {
                assert!(String::from_utf8_lossy(&request).contains(range));
            }
            socket.write_all(response.as_bytes()).unwrap();
        });
        (url, thread)
    }

    fn media(url: String) -> Media {
        Media {
            id: "test".into(),
            filename: "a.jpg".into(),
            url,
            size: 6,
            fingerprint: "v1".into(),
            asset_date_ms: None,
            preview_url: None,
        }
    }

    #[tokio::test]
    async fn resumes_and_skips_verified_files() {
        let dir = tempfile::tempdir().unwrap();
        let (url, thread) = server(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
            Some("range: bytes=3-"),
        );
        let m = media(url);
        let dest = destination(dir.path(), &m);
        fs::write(
            dest.with_file_name(format!(
                "{}.part",
                dest.file_name().unwrap().to_string_lossy()
            )),
            b"abc",
        )
        .await
        .unwrap();
        let client = Client::new();
        let mut updates = Vec::new();
        assert_eq!(
            download_with_progress(&client, dir.path(), &m, 0, |bytes, total| {
                updates.push((bytes, total));
            })
            .await
            .unwrap(),
            Outcome::Downloaded
        );
        assert_eq!(updates.first(), Some(&(0, 6)));
        assert!(updates.contains(&(3, 6)), "report resumed bytes");
        assert_eq!(updates.last(), Some(&(6, 6)));
        thread.join().unwrap();
        assert_eq!(fs::read(&dest).await.unwrap(), b"abcdef");
        updates.clear();
        assert_eq!(
            download_with_progress(&client, dir.path(), &m, 0, |bytes, total| {
                updates.push((bytes, total));
            })
            .await
            .unwrap(),
            Outcome::Skipped
        );
        assert_eq!(updates, [(0, 6), (6, 6)]);
        fs::write(&dest, b"broken").await.unwrap();
        assert!(download(&client, dir.path(), &m, 0).await.is_err());
    }

    #[tokio::test]
    async fn restarts_when_server_ignores_range() {
        let dir = tempfile::tempdir().unwrap();
        let (url, thread) = server(
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef",
            Some("range: bytes=3-"),
        );
        let m = media(url);
        let dest = destination(dir.path(), &m);
        fs::write(
            dest.with_file_name(format!(
                "{}.part",
                dest.file_name().unwrap().to_string_lossy()
            )),
            b"abc",
        )
        .await
        .unwrap();
        let mut updates = Vec::new();
        download_with_progress(&Client::new(), dir.path(), &m, 0, |bytes, total| {
            updates.push((bytes, total));
        })
        .await
        .unwrap();
        thread.join().unwrap();
        assert_eq!(fs::read(dest).await.unwrap(), b"abcdef");
        assert_eq!(updates.first(), Some(&(0, 6)));
        assert!(
            !updates.contains(&(3, 6)),
            "ignored Range restarts from zero"
        );
        assert_eq!(updates.last(), Some(&(6, 6)));
    }

    #[test]
    fn protects_paths_and_validates_ranges() {
        assert!(!safe_name("../../旅行\\a.jpg").contains('/'));
        assert_eq!(safe_name("..."), "unnamed");
        assert!(safe_name(&"中".repeat(200)).len() <= 170);
        assert!(range_matches("bytes 3-5/6", 3, 6));
        assert!(!range_matches("bytes 0-5/6", 3, 6));
        assert!(!range_matches("bytes 3-4/6", 3, 6));
        let a = media(String::new());
        let mut b = a.clone();
        b.id = "different".into();
        assert_ne!(
            destination(Path::new("/tmp"), &a),
            destination(Path::new("/tmp"), &b)
        );
    }

    #[tokio::test]
    async fn rejects_incorrect_range_without_touching_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (url, thread) = server(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/6\r\nConnection: close\r\n\r\ndef",
            Some("range: bytes=3-"),
        );
        let m = media(url);
        let dest = destination(dir.path(), &m);
        let part = dest.with_file_name(format!(
            "{}.part",
            dest.file_name().unwrap().to_string_lossy()
        ));
        fs::write(&part, b"abc").await.unwrap();
        assert!(download(&Client::new(), dir.path(), &m, 0).await.is_err());
        thread.join().unwrap();
        assert!(!dest.exists());
        assert_eq!(fs::read(part).await.unwrap(), b"abc");
    }
}

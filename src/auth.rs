use crate::srp::{Challenge, Srp};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Client, Method, Response,
    cookie::Jar,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroizing;

const WIDGET: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
// Apple Account authentication is global, including mainland China accounts.
// Only the iCloud home/setup/data services use the regional .com.cn suffix.
// https://pyicloud.readthedocs.io/en/latest/api/pyicloud-service/
const AUTH_ROOT: &str = "https://idmsa.apple.com";

#[derive(Serialize, Deserialize)]
struct Tokens {
    username: String,
    domain: String,
    client_id: String,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Clone, Copy)]
pub enum AuthInput {
    Password,
    Code,
}

pub struct Session {
    pub client: Client,
    auth_client: Client,
    pub home: String,
    tokens: Tokens,
    path: PathBuf,
    auth_root: String,
    setup: String,
}

pub fn prompt(label: &str) -> Result<String> {
    ensure!(
        io::stdin().is_terminal(),
        "需要交互输入：{label}；请在终端运行，或设置对应环境变量"
    );
    print!("{label}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("缺少父目录")?;
    std::fs::create_dir_all(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // rename atomically replaces on Unix; on Windows token refresh needs removal first.
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

impl Session {
    pub fn new(username: &str, domain: &str, session_dir: &Path) -> Result<Self> {
        let id = format!(
            "{:x}",
            Sha256::digest(format!("{domain}:{username}").as_bytes())
        );
        let path = session_dir.join(format!("{id}.json"));
        let tokens = if path.exists() {
            serde_json::from_slice::<Tokens>(&std::fs::read(&path)?)
                .context("会话文件无效，请删除后重试")?
        } else {
            Tokens {
                username: username.into(),
                domain: domain.into(),
                client_id: Uuid::new_v4().to_string(),
                headers: HashMap::new(),
            }
        };
        ensure!(
            tokens.username == username && tokens.domain == domain,
            "会话账号不匹配"
        );
        let suffix = if domain == "cn" { "com.cn" } else { "com" };
        let home = format!("https://www.icloud.{suffix}");
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_str(&home)?);
        headers.insert("Referer", HeaderValue::from_str(&format!("{home}/"))?);
        let cookies = Arc::new(Jar::default());
        let builder = || {
            Client::builder()
                .cookie_provider(Arc::clone(&cookies))
                .default_headers(headers.clone())
                .user_agent("Mozilla/5.0 iCloudDownloadHelper/0.1")
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(120))
        };
        let client = builder().build()?;
        // Keep authentication failures at their original endpoint. Following a
        // redirect can hide a 302 behind an HTML 404 and may forward auth headers.
        // Share the cookie jar with the data client for accountLogin/CloudKit.
        let auth_client = builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            auth_client,
            tokens,
            path,
            home,
            auth_root: AUTH_ROOT.into(),
            setup: format!("https://setup.icloud.{suffix}/setup/ws/1"),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.tokens.client_id
    }

    fn auth_url(&self, path: &str) -> String {
        format!("{}/appleauth/auth/{path}", self.auth_root)
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        for (name, value) in [
            // IDMS otherwise treats the request as a page navigation and can
            // redirect to www.apple.com/filenotfound instead of returning JSON.
            ("Accept", "application/json, text/javascript"),
            ("Content-Type", "application/json"),
            ("X-Apple-OAuth-Client-Id", WIDGET),
            ("X-Apple-Widget-Key", WIDGET),
            ("X-Apple-OAuth-Client-Type", "firstPartyAuth"),
            ("X-Apple-OAuth-Redirect-URI", &self.home),
            ("X-Apple-OAuth-Require-Grant-Code", "true"),
            ("X-Apple-OAuth-Response-Mode", "web_message"),
            ("X-Apple-OAuth-Response-Type", "code"),
            ("X-Apple-OAuth-State", &self.tokens.client_id),
            ("Origin", &self.auth_root),
            ("Referer", &self.auth_root),
        ] {
            h.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
        for name in ["scnt", "x-apple-id-session-id"] {
            if let Some(v) = self.tokens.headers.get(name) {
                h.insert(
                    reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                    HeaderValue::from_str(v)?,
                );
            }
        }
        Ok(h)
    }

    async fn auth_request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        let mut request = self
            .auth_client
            .request(method, self.auth_url(path))
            .headers(self.headers()?);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("连接 Apple 认证服务失败")?;
        if response.status().is_redirection() {
            let target = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| response.url().join(value).ok())
                .map(|url| endpoint_label(&url))
                .unwrap_or_else(|| "未提供有效目标地址".into());
            bail!(
                "Apple 认证接口返回重定向（HTTP {}）：{} → {}；已停止跟随，请检查认证请求头或网络代理",
                response.status().as_u16(),
                endpoint_label(response.url()),
                target
            );
        }
        self.capture(&response);
        Ok(response)
    }

    fn capture(&mut self, response: &Response) {
        for name in [
            "scnt",
            "x-apple-id-session-id",
            "x-apple-session-token",
            "x-apple-twosv-trust-token",
            "x-apple-id-account-country",
        ] {
            if let Some(value) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
                self.tokens.headers.insert(name.into(), value.into());
            }
        }
    }

    async fn account_login(&mut self) -> Result<Value> {
        let h = &self.tokens.headers;
        ensure!(
            h.contains_key("x-apple-session-token"),
            "Apple 未提供会话令牌，请在网页端检查账号或重试登录"
        );
        let response = self
            .client
            .post(format!("{}/accountLogin", self.setup))
            .json(&json!({
            "accountCountryCode": h.get("x-apple-id-account-country").map(String::as_str).unwrap_or(""),
            "dsWebAuthToken": h.get("x-apple-session-token").map(String::as_str).unwrap_or(""),
            "trustToken": h.get("x-apple-twosv-trust-token").map(String::as_str).unwrap_or(""), "extended_login": true
            }))
            .send()
            .await
            .context("iCloud accountLogin 连接失败")?;
        self.capture(&response);
        checked_json(response, "iCloud accountLogin").await
    }

    pub async fn login(&mut self, fresh: bool) -> Result<Value> {
        self.login_with(
            fresh,
            std::env::var_os("ICLOUD_2FA_CODE").is_none(),
            |kind| match kind {
                AuthInput::Password => Ok(match std::env::var("ICLOUD_PASSWORD") {
                    Ok(v) => v,
                    Err(_) => {
                        ensure!(
                            io::stdin().is_terminal(),
                            "请在终端输入密码，或设置 ICLOUD_PASSWORD"
                        );
                        dialoguer::Password::new()
                            .with_prompt("Apple 账号密码（非 App 专用密码）")
                            .interact()?
                    }
                }),
                AuthInput::Code => match std::env::var("ICLOUD_2FA_CODE") {
                    Ok(v) => Ok(v),
                    Err(_) => prompt("六位验证码"),
                },
            },
        )
        .await
    }

    pub async fn login_with(
        &mut self,
        fresh: bool,
        request_code: bool,
        mut input: impl FnMut(AuthInput) -> Result<String>,
    ) -> Result<Value> {
        if !fresh && self.tokens.headers.contains_key("x-apple-session-token") {
            match self.account_login().await {
                Ok(data) if !requires_2fa(&data) => {
                    self.save()?;
                    return Ok(data);
                }
                Ok(_) => {}
                Err(error) => eprintln!("现有会话不可用，将重新登录：{error}"),
            }
        }
        self.tokens
            .headers
            .retain(|k, _| k == "x-apple-twosv-trust-token");
        let password = Zeroizing::new(input(AuthInput::Password)?);
        ensure!(!password.is_empty(), "密码不能为空");
        let srp = Srp::new();
        let init = self.auth_request(Method::POST, "signin/init", Some(json!({"a":srp.public_key(),"accountName": self.tokens.username,"protocols":["s2k","s2k_fo"]}))).await?;
        let challenge: Challenge = serde_json::from_value(checked_json(init, "SRP 初始化").await?)
            .context("Apple 返回了无法识别的 SRP challenge")?;
        let (m1, m2) = srp.proofs(&self.tokens.username, &password, &challenge)?;
        drop(password);
        let trust: Vec<_> = self
            .tokens
            .headers
            .get("x-apple-twosv-trust-token")
            .into_iter()
            .cloned()
            .collect();
        let complete = self.auth_request(Method::POST, "signin/complete?isRememberMeEnabled=true", Some(json!({"accountName":self.tokens.username,"c":challenge.c,"m1":m1,"m2":m2,"rememberMe":true,"trustTokens":trust}))).await?;
        let challenge_required = complete.status().as_u16() == 409;
        if challenge_required {
            let data: Value = complete.json().await.context("Apple 双重认证响应无效")?;
            ensure!(
                data.get("fsaChallenge").is_none() && data.get("keyNames").is_none(),
                "此版本不支持硬件安全密钥认证"
            );
        } else if !complete.status().is_success() {
            bail!(
                "Apple 登录失败（HTTP {}），请检查账号、密码、区域或网页端账号状态",
                complete.status().as_u16()
            );
        }
        let mut data = self.account_login().await?;
        if challenge_required || requires_2fa(&data) {
            eprintln!("请在受信任的 Apple 设备上允许登录，并输入六位验证码。");
            if request_code {
                let response = self
                    .auth_request(Method::PUT, "verify/trusteddevice/securitycode", None)
                    .await?;
                ensure!(
                    response.status().is_success(),
                    "请求验证码失败（HTTP {}）",
                    response.status().as_u16()
                );
            }
            let code = input(AuthInput::Code)?;
            ensure!(
                code.len() == 6 && code.bytes().all(|c| c.is_ascii_digit()),
                "验证码须为六位数字"
            );
            let response = self
                .auth_request(
                    Method::POST,
                    "verify/trusteddevice/securitycode",
                    Some(json!({"securityCode":{"code":code}})),
                )
                .await?;
            ensure!(
                response.status().is_success(),
                "验证码验证失败（HTTP {}）",
                response.status().as_u16()
            );
            let response = self.auth_request(Method::GET, "2sv/trust", None).await?;
            ensure!(
                response.status().is_success(),
                "信任会话失败（HTTP {}）",
                response.status().as_u16()
            );
            data = self.account_login().await?;
        }
        ensure!(
            !requires_2fa(&data),
            "Apple 仍要求验证账号，请在 iCloud 网页完成验证后重试"
        );
        self.save()?;
        Ok(data)
    }

    fn save(&self) -> Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(&self.tokens)?).context("保存会话失败")
    }
}

fn requires_2fa(v: &Value) -> bool {
    v["dsInfo"]["hsaVersion"].as_i64() == Some(2)
        && (v["hsaChallengeRequired"].as_bool() == Some(true)
            || v["hsaTrustedBrowser"].as_bool() != Some(true))
}

fn endpoint_label(url: &reqwest::Url) -> String {
    // Never expose URL credentials, account identifiers or tokens in queries.
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

pub async fn checked_json(response: Response, operation: &str) -> Result<Value> {
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // Exclude query parameters, which may contain account identifiers.
        bail!(
            "{operation}失败（HTTP 404）；接口地址不存在或已变更：{}",
            endpoint_label(response.url())
        );
    }
    ensure!(
        status.is_success(),
        "{operation} 失败（HTTP {}）；检查登录区域、网页数据访问权限和账号状态",
        status.as_u16()
    );
    let data: Value = response
        .json()
        .await
        .with_context(|| format!("{operation} 返回的不是有效 JSON"))?;
    if let Some(code) = data.get("serverErrorCode").and_then(Value::as_str) {
        bail!("{operation}：{code}；请确认已允许 iCloud 网页数据访问；高级数据保护可能阻止此接口");
    }
    ensure!(
        data.get("domainToUse").is_none(),
        "账号要求使用另一地区，请切换 --domain com/cn"
    );
    ensure!(
        data.get("serviceErrors")
            .and_then(Value::as_array)
            .is_none_or(|e| e.is_empty()),
        "{operation}：Apple 返回服务错误，请在网页端检查账号状态"
    );
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_server(responses: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_ne!(socket.read(&mut byte).unwrap(), 0);
                    request.push(byte[0]);
                }
                let header = String::from_utf8(request.clone()).unwrap();
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                request.extend(body);
                requests.push(String::from_utf8(request).unwrap());
                socket.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (url, worker)
    }

    #[tokio::test]
    async fn sends_json_accept_and_content_type_on_auth_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("test@icloud.com", "cn", dir.path()).unwrap();
        let (url, server) = mock_server(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"iteration\":1}",
        ]);
        session.auth_root = url;
        let response = session
            .auth_request(Method::POST, "signin/init", Some(json!({})))
            .await
            .unwrap();
        assert_eq!(
            checked_json(response, "SRP 初始化").await.unwrap()["iteration"],
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /appleauth/auth/signin/init HTTP/1.1\r\n"));
        let headers = requests[0].to_ascii_lowercase();
        assert!(headers.contains("\r\naccept: application/json, text/javascript\r\n"));
        assert!(headers.contains("\r\ncontent-type: application/json\r\n"));
    }

    // Optional live routing check. Sends an empty JSON object, never the sample
    // username or a password, and does not attempt account authentication.
    #[tokio::test]
    #[ignore = "requires access to Apple's public authentication endpoint"]
    async fn apple_srp_route_returns_json_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("unused@example.invalid", "cn", dir.path()).unwrap();
        let response = session
            .auth_request(Method::POST, "signin/init", Some(json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let data = checked_json(response, "SRP 路由探测").await.unwrap();
        assert!(data["iteration"].is_number());
    }

    #[tokio::test]
    async fn rejects_auth_redirect_at_original_endpoint_and_redacts_queries() {
        for status in ["302 Found", "307 Temporary Redirect"] {
            let response = if status.starts_with("302") {
                "HTTP/1.1 302 Found\r\nLocation: /filenotfound?token=private-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/filenotfound?token=private-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let (url, server) = mock_server(vec![response]);
            let dir = tempfile::tempdir().unwrap();
            let mut session = Session::new("test@icloud.com", "cn", dir.path()).unwrap();
            session.auth_root = url;
            let message = session
                .auth_request(
                    Method::POST,
                    "signin/init?account=private-account",
                    Some(json!({})),
                )
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(server.join().unwrap().len(), 1);
            assert!(message.contains(&format!("HTTP {}", &status[..3])));
            assert!(message.contains("/appleauth/auth/signin/init"));
            assert!(message.contains("/filenotfound"));
            assert!(!message.contains("private-account"));
            assert!(!message.contains("private-token"));
        }
    }

    #[tokio::test]
    async fn auth_and_data_clients_share_session_cookies() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("test@icloud.com", "cn", dir.path()).unwrap();
        let (url, server) = mock_server(vec![
            "HTTP/1.1 200 OK\r\nSet-Cookie: test-session=shared-cookie; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ]);
        session.auth_root = url.clone();
        session
            .auth_request(Method::POST, "signin/init", Some(json!({})))
            .await
            .unwrap();
        session
            .client
            .get(format!("{url}/accountLogin"))
            .send()
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert!(!requests[0].contains("test-session=shared-cookie"));
        assert!(requests[1].contains("test-session=shared-cookie"));
    }

    #[test]
    fn uses_global_authentication_with_regional_data_services() {
        let dir = tempfile::tempdir().unwrap();
        for (domain, suffix) in [("com", "com"), ("cn", "com.cn")] {
            let session = Session::new("test@icloud.com", domain, dir.path()).unwrap();
            for path in [
                "signin/init",
                "signin/complete?isRememberMeEnabled=true",
                "verify/trusteddevice/securitycode",
                "2sv/trust",
            ] {
                assert_eq!(
                    session.auth_url(path),
                    format!("https://idmsa.apple.com/appleauth/auth/{path}")
                );
            }
            assert_eq!(session.home, format!("https://www.icloud.{suffix}"));
            assert_eq!(
                session.setup,
                format!("https://setup.icloud.{suffix}/setup/ws/1")
            );
            let headers = session.headers().unwrap();
            assert_eq!(headers["Origin"], "https://idmsa.apple.com");
            assert_eq!(headers["X-Apple-OAuth-Redirect-URI"], session.home);
        }
    }

    #[tokio::test]
    async fn reports_missing_endpoint_without_account_query_parameters() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_ne!(socket.read(&mut byte).unwrap(), 0);
                request.push(byte[0]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let response = Client::new()
            .get(format!(
                "http://{address}/appleauth/auth/signin/init?dsid=private-account-id"
            ))
            .send()
            .await
            .unwrap();
        let message = checked_json(response, "SRP 初始化")
            .await
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(message.contains("HTTP 404"));
        assert!(message.contains("/appleauth/auth/signin/init"));
        assert!(!message.contains("private-account-id"));
        assert!(!message.contains("密码"));
    }

    #[test]
    fn detects_untrusted_and_challenged_sessions() {
        assert!(requires_2fa(
            &json!({"dsInfo":{"hsaVersion":2},"hsaTrustedBrowser":false})
        ));
        assert!(requires_2fa(
            &json!({"dsInfo":{"hsaVersion":2},"hsaTrustedBrowser":true,"hsaChallengeRequired":true})
        ));
        assert!(!requires_2fa(
            &json!({"dsInfo":{"hsaVersion":2},"hsaTrustedBrowser":true})
        ));
    }

    #[test]
    fn isolates_accounts_and_regions_and_persists_private_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new("test@icloud.com", "cn", dir.path()).unwrap();
        assert_eq!(s.auth_root, "https://idmsa.apple.com");
        assert_ne!(
            s.path,
            Session::new("test@icloud.com", "com", dir.path())
                .unwrap()
                .path
        );
        assert_ne!(
            s.path,
            Session::new("other@icloud.com", "cn", dir.path())
                .unwrap()
                .path
        );
        s.tokens
            .headers
            .insert("x-apple-session-token".into(), "test-token".into());
        s.save().unwrap();
        s.save().unwrap();
        let restored = Session::new("test@icloud.com", "cn", dir.path()).unwrap();
        assert_eq!(
            restored.tokens.headers["x-apple-session-token"],
            "test-token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(s.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

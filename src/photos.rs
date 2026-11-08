use crate::auth::{Session, checked_json};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct Album {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Media {
    pub id: String,
    pub filename: String,
    pub url: String,
    pub size: u64,
    pub fingerprint: String,
}

pub struct Page {
    pub media: Vec<Media>,
    pub masters: usize,
    pub empty: bool,
}

pub struct Photos<'a> {
    session: &'a Session,
    endpoint: String,
    dsid: String,
}

pub fn field<'a>(record: &'a Value, key: &str) -> &'a Value {
    &record["fields"][key]["value"]
}

fn decode_name(record: &Value, key: &str) -> Option<String> {
    String::from_utf8(B64.decode(field(record, key).as_str()?).ok()?).ok()
}

impl<'a> Photos<'a> {
    pub fn new(session: &'a Session, account: &Value) -> Result<Self> {
        let root = account["webservices"]["ckdatabasews"]["url"]
            .as_str()
            .context("账号没有提供 iCloud 照片接口，请确认已开启 iCloud 照片和网页数据访问")?;
        let url = reqwest::Url::parse(root)?;
        ensure!(url.scheme() == "https", "iCloud 照片接口必须使用 HTTPS");
        let dsid = match &account["dsInfo"]["dsid"] {
            Value::String(v) => v.clone(),
            Value::Number(v) => v.to_string(),
            _ => anyhow::bail!("Apple 响应缺少 dsid"),
        };
        Ok(Self {
            session,
            endpoint: format!(
                "{}/database/1/com.apple.photos.cloud/production/private",
                root.trim_end_matches('/')
            ),
            dsid,
        })
    }

    async fn query(&self, body: &Value) -> Result<Value> {
        for attempt in 0..4 {
            let response = self
                .session
                .client
                .post(format!("{}/records/query", self.endpoint))
                .query(&[
                    ("dsid", self.dsid.as_str()),
                    ("clientId", self.session.client_id()),
                    ("remapEnums", "true"),
                    ("getCurrentSyncToken", "true"),
                ])
                .json(body)
                .send()
                .await;
            match response {
                Ok(r)
                    if (r.status().is_server_error() || r.status().as_u16() == 429)
                        && attempt < 3 => {}
                Ok(r) => return checked_json(r, "读取照片库").await,
                Err(_) if attempt < 3 => {}
                Err(e) => return Err(e.without_url()).context("连接 iCloud 照片库失败"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
        }
        unreachable!()
    }

    pub async fn check_index(&self) -> Result<()> {
        let data = self.query(&json!({"query":{"recordType":"CheckIndexingState"},"zoneID":{"zoneName":"PrimarySync"}})).await?;
        let state = data["records"]
            .as_array()
            .and_then(|r| r.first())
            .and_then(|r| field(r, "state").as_str());
        ensure!(
            state == Some("FINISHED"),
            "iCloud 照片索引尚未就绪（{:?}），请先打开 iCloud 网页照片并稍后重试",
            state
        );
        Ok(())
    }

    pub async fn albums(&self) -> Result<Vec<Album>> {
        let mut albums = Vec::new();
        let mut marker: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let mut body = json!({"query":{"recordType":"CPLAlbumByPositionLive"},"zoneID":{"zoneName":"PrimarySync"},"resultsLimit":200});
            if let Some(m) = &marker {
                body["continuationMarker"] = json!(m);
            }
            let data = self.query(&body).await?;
            for record in data["records"].as_array().context("相册响应缺少 records")? {
                let Some(id) = record["recordName"].as_str() else {
                    continue;
                };
                if id.starts_with("----") || field(record, "isDeleted").as_bool() == Some(true) {
                    continue;
                }
                if let Some(name) = decode_name(record, "albumNameEnc") {
                    albums.push(Album {
                        id: id.into(),
                        name,
                    });
                }
            }
            marker = data["continuationMarker"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(str::to_owned);
            match &marker {
                Some(m) => ensure!(
                    seen.insert(m.clone()),
                    "Apple 返回重复分页标记，停止以避免漏报相册"
                ),
                None => break,
            }
        }
        albums.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        albums.dedup_by(|a, b| a.id == b.id);
        Ok(albums)
    }

    pub async fn page(&self, album: Option<&Album>, offset: u64, live: bool) -> Result<Page> {
        let mut filter = vec![
            json!({"fieldName":"startRank","comparator":"EQUALS","fieldValue":{"type":"INT64","value":offset}}),
            json!({"fieldName":"direction","comparator":"EQUALS","fieldValue":{"type":"STRING","value":"ASCENDING"}}),
        ];
        if let Some(a) = album {
            filter.push(json!({"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":a.id}}));
        }
        let kind = if album.is_some() {
            "CPLContainerRelationLiveByAssetDate"
        } else {
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
        };
        let data = self.query(&json!({"query":{"recordType":kind,"filterBy":filter},"resultsLimit":200,
            "desiredKeys":["filenameEnc","resOriginalRes","resOriginalFileType","resOriginalVidComplRes","resOriginalVidComplFileType","itemType","masterRef"],
            "zoneID":{"zoneName":"PrimarySync"}})).await?;
        parse_page(&data, live)
    }
}

fn parse_page(data: &Value, live: bool) -> Result<Page> {
    let records = data["records"].as_array().context("照片响应缺少 records")?;
    let mut result = Page {
        media: Vec::new(),
        masters: 0,
        empty: records.is_empty(),
    };
    for record in records {
        ensure!(
            record.get("serverErrorCode").is_none(),
            "照片记录返回错误，停止以免跳过文件"
        );
        if record["recordType"] != "CPLMaster" {
            continue;
        }
        result.masters += 1;
        let id = record["recordName"]
            .as_str()
            .context("照片缺少 recordName")?;
        let name = decode_name(record, "filenameEnc").context("照片缺少有效原始文件名")?;
        result.media.push(parse_resource(
            record,
            "resOriginalRes",
            id.into(),
            name.clone(),
        )?);
        if live && !field(record, "resOriginalVidComplRes").is_null() {
            let stem = std::path::Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("live");
            result.media.push(parse_resource(
                record,
                "resOriginalVidComplRes",
                format!("{id}:live"),
                format!("{stem}_LIVE.MOV"),
            )?);
        }
    }
    ensure!(
        result.empty || result.masters > 0,
        "Apple 返回照片记录但没有原始资源，停止以避免不完整下载"
    );
    Ok(result)
}

fn parse_resource(record: &Value, key: &str, id: String, filename: String) -> Result<Media> {
    let resource = field(record, key);
    let url = resource["downloadURL"]
        .as_str()
        .context("原始资源缺少下载地址")?;
    ensure!(
        reqwest::Url::parse(url)?.scheme() == "https",
        "照片下载地址必须使用 HTTPS"
    );
    let size = resource["size"].as_u64().context("原始资源缺少文件大小")?;
    ensure!(size > 0, "原始资源大小为零");
    Ok(Media {
        id,
        filename,
        url: url.into(),
        size,
        fingerprint: resource["fileChecksum"].as_str().unwrap_or("").into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_original_and_live_and_rejects_missing_original() {
        let mut page = json!({"records":[{"recordType":"CPLAsset"},{"recordType":"CPLMaster","recordName":"id", "fields":{
            "filenameEnc":{"value":B64.encode("旅行.HEIC")},
            "resOriginalRes":{"value":{"downloadURL":"https://example.com/a","size":12}},
            "resOriginalVidComplRes":{"value":{"downloadURL":"https://example.com/b","size":24}}
        }}]});
        let parsed = parse_page(&page, true).unwrap();
        assert_eq!(parsed.masters, 1);
        assert_eq!(parsed.media.len(), 2);
        assert_eq!(parsed.media[0].filename, "旅行.HEIC");
        assert_eq!(parsed.media[1].filename, "旅行_LIVE.MOV");
        page["records"][1]["fields"]["resOriginalRes"] = Value::Null;
        assert!(parse_page(&page, true).is_err());
    }
}

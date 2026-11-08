use crate::photos::Media;
use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;

/// Inclusive UTC date range and a cap on selected files, including existing or failed files.
#[derive(Clone, Debug)]
pub struct DownloadFilter {
    start_ms: Option<i64>,
    end_exclusive_ms: Option<i64>,
    limit: Option<usize>,
    selected: usize,
}

impl DownloadFilter {
    pub fn new(start: &str, end: &str, limit: &str) -> Result<Self> {
        fn date(value: &str) -> Result<Option<NaiveDate>> {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            ensure!(
                value.len() == 10 && value.as_bytes()[4] == b'-' && value.as_bytes()[7] == b'-',
                "日期格式必须为 YYYY-MM-DD"
            );
            Ok(Some(
                NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .context("日期无效，格式须为 YYYY-MM-DD")?,
            ))
        }
        let start = date(start)?;
        let end = date(end)?;
        if let (Some(start), Some(end)) = (start, end) {
            ensure!(start <= end, "开始日期不能晚于结束日期");
        }
        let start_ms = start.map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis());
        let end_exclusive_ms = end
            .map(|d| {
                d.succ_opt()
                    .context("结束日期超出范围")
                    .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
            })
            .transpose()?;
        let limit = if limit.trim().is_empty() {
            None
        } else {
            let count: usize = limit.trim().parse().context("文件数量上限必须是正整数")?;
            ensure!(count > 0, "文件数量上限必须是正整数");
            Some(count)
        };
        Ok(Self {
            start_ms,
            end_exclusive_ms,
            limit,
            selected: 0,
        })
    }

    pub fn full(&self) -> bool {
        self.limit.is_some_and(|limit| self.selected >= limit)
    }

    pub fn select(&mut self, media: &Media) -> Result<bool> {
        if self.full() {
            return Ok(false);
        }
        if self.start_ms.is_some() || self.end_exclusive_ms.is_some() {
            let date = media
                .asset_date_ms
                .with_context(|| format!("{} 缺少拍摄日期，无法按日期筛选", media.filename))?;
            if self.start_ms.is_some_and(|start| date < start)
                || self.end_exclusive_ms.is_some_and(|end| date >= end)
            {
                return Ok(false);
            }
        }
        self.selected += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn media(date: Option<i64>) -> Media {
        Media {
            id: "id".into(),
            filename: "test.jpg".into(),
            url: String::new(),
            size: 1,
            fingerprint: String::new(),
            preview_url: None,
            asset_date_ms: date,
        }
    }
    #[test]
    fn validates_dates_and_limits() {
        for (start, end, limit) in [
            ("2024-02-30", "", ""),
            ("2024-2-01", "", ""),
            ("2024-03-01", "2024-02-01", ""),
            ("", "", "0"),
            ("", "", "-1"),
        ] {
            assert!(DownloadFilter::new(start, end, limit).is_err());
        }
    }
    #[test]
    fn filters_utc_boundaries_and_counts_only_matches() {
        let mut f = DownloadFilter::new("2024-01-01", "2024-01-01", "2").unwrap();
        assert!(!f.select(&media(Some(1704067199999))).unwrap());
        assert!(f.select(&media(Some(1704067200000))).unwrap());
        assert!(!f.select(&media(Some(1704153600000))).unwrap());
        assert!(f.select(&media(Some(1704153599999))).unwrap());
        assert!(f.full());
        assert!(!f.select(&media(Some(1704067200000))).unwrap());
        assert!(
            DownloadFilter::new("2024-01-01", "", "")
                .unwrap()
                .select(&media(None))
                .is_err()
        );
        assert!(
            DownloadFilter::new("", "", "")
                .unwrap()
                .select(&media(None))
                .unwrap()
        );
    }
}

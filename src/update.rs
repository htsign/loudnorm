//! Checking GitHub for a newer release.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// The API endpoint that describes the latest published (non-draft, non-prerelease) release.
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/htsign/loudnorm/releases/latest";

/// Every URL taken from a release (its page) must start with this, so a response cannot point the
/// app at another site or repository.
const RELEASES_URL_PREFIX: &str = "https://github.com/htsign/loudnorm/releases/";

/// Every download URL of a release file is this followed by `v<version>/<file name>`, for the same
/// reason.
const DOWNLOAD_URL_PREFIX: &str = "https://github.com/htsign/loudnorm/releases/download/";

/// Upper bound for the whole request (connect, send and read).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A release version `major.minor.patch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `1.2.3` or `v1.2.3`. Anything else (pre-release suffixes, signs, a different number of
    /// components, numbers that overflow `u64`) is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let mut parts = s.strip_prefix('v').unwrap_or(s).split('.');
        let mut next = || {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse::<u64>().ok()
        };
        let (major, minor, patch) = (next()?, next()?, next()?);
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A file attached to a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// The file name (`name`).
    pub name: String,
    /// Where the file is downloaded from (`browser_download_url`).
    pub url: String,
}

/// The latest published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version the release's tag names.
    pub version: Version,
    /// The release's page on GitHub (`html_url`).
    pub url: String,
    /// The files attached to the release.
    pub assets: Vec<Asset>,
}

/// The fields of GitHub's release object that are used; the rest of the response is ignored.
#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Option<Vec<AssetJson>>,
}

/// The fields of a release's file that are used.
#[derive(Deserialize)]
struct AssetJson {
    name: Option<String>,
    browser_download_url: Option<String>,
}

/// Reads a GitHub "release" response. `None` when the body is not valid JSON, is a draft or a
/// pre-release, has no usable `tag_name` / `html_url` (e.g. a "Not Found" error body), or when its
/// `html_url` is not a release page of this repository.
pub fn latest_from_json(body: &str) -> Option<Release> {
    let r: ReleaseJson = serde_json::from_str(body).ok()?;
    if r.draft || r.prerelease {
        return None;
    }
    let url = r.html_url.filter(|u| is_release_url(u))?;
    let assets = r
        .assets
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| Some(Asset { name: a.name?, url: a.browser_download_url? }))
        .collect();
    Some(Release { version: Version::parse(&r.tag_name?)?, url, assets })
}

/// Whether `u` is a page under this repository's releases that a browser or HTTP client resolves
/// to the same place the text says. Only ASCII letters, digits and `-._/` may follow the prefix, so
/// a query, a fragment, a backslash or a percent-encoded `..` cannot be used to leave the
/// repository, and no path segment may be empty, `.` or `..`.
fn is_release_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix(RELEASES_URL_PREFIX) else {
        return false;
    };
    rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'/'))
        && rest.split('/').all(|seg| !matches!(seg, "" | "." | ".."))
}

/// Whether `latest` is strictly greater than `current`.
pub fn is_newer(latest: &Version, current: &Version) -> bool {
    latest > current
}

/// The name of the release file for `target` (`loudnorm-v<version>-<target>.<zip|tar.gz>`), or
/// `None` when `target` is not a Windows MSVC, macOS or Linux GNU target.
pub fn asset_name(version: &Version, target: &str) -> Option<String> {
    let ext = if target.ends_with("-pc-windows-msvc") || target.ends_with("-apple-darwin") {
        "zip"
    } else if target.ends_with("-unknown-linux-gnu") {
        "tar.gz"
    } else {
        return None;
    };
    Some(format!("loudnorm-v{version}-{target}.{ext}"))
}

/// The download URL of the asset named exactly `name`, or `None` when there is no such asset or its
/// URL is not exactly this repository's download URL of `name` under the release's own tag (the
/// workflow tags every release `v<version>`), so a response cannot point at an older release.
pub fn download_url<'a>(release: &'a Release, name: &str) -> Option<&'a str> {
    let asset = release.assets.iter().find(|a| a.name == name)?;
    let expected = format!("{DOWNLOAD_URL_PREFIX}v{}/{name}", release.version);
    (asset.url == expected).then_some(asset.url.as_str())
}

/// The SHA-256 of `file` listed in the text of a `SHA256SUMS.txt` (lines `<hex>  <name>` or
/// `<hex> *<name>`, as `sha256sum` writes them), or `None` when no valid line names `file`.
pub fn sha256_from_sums(sums: &str, file: &str) -> Option<[u8; 32]> {
    sums.split('\n').find_map(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (hex, rest) = line.split_at_checked(64)?;
        let name = rest.strip_prefix("  ").or_else(|| rest.strip_prefix(" *"))?;
        if name != file {
            return None;
        }
        decode_sha256(hex)
    })
}

/// The 32 bytes written as 64 hex digits (either case), or `None` for any other text.
fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    let digits = hex.bytes().map(|b| char::from(b).to_digit(16)).collect::<Option<Vec<_>>>()?;
    let mut out = [0u8; 32];
    if digits.len() != out.len() * 2 {
        return None;
    }
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = ((pair[0] << 4) | pair[1]) as u8;
    }
    Some(out)
}

/// What the last finished check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    NotChecked,
    UpToDate,
    Available(Release),
    Failed(String),
}

/// The outcome after a check finishes; `result` is the fetched release or the error message.
/// A failed re-check keeps an already known newer release instead of replacing it with the failure.
pub fn next_outcome(prev: &Outcome, current: &Version, result: Result<Release, String>) -> Outcome {
    match result {
        Ok(r) if is_newer(&r.version, current) => Outcome::Available(r),
        Ok(_) => Outcome::UpToDate,
        Err(e) => match prev {
            Outcome::Available(_) => prev.clone(),
            _ => Outcome::Failed(e),
        },
    }
}

/// Asks GitHub for the latest release. Blocks until the answer arrives or the request times out, so
/// call it off the UI thread.
pub fn fetch_latest() -> Result<Release> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(REQUEST_TIMEOUT)).build().into();
    let mut resp = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("loudnorm/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => anyhow!("GitHub が HTTP {code} を返しました"),
            e => anyhow!("通信に失敗しました: {e}"),
        })?;
    let body = resp.body_mut().read_to_string().context("応答を読み取れませんでした")?;
    latest_from_json(&body).ok_or_else(|| anyhow!("最新のリリースを読み取れませんでした"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version { major, minor, patch }
    }

    /// A "latest release" response in the shape GitHub returns. The author and the assets carry
    /// their own `url` / `html_url` / `name` keys before and after the release's own keys, the
    /// release `name` differs from the tag, and strings contain braces and brackets. The `body`
    /// comes before the release's own keys and holds an unclosed `{` and `]`, an odd number of
    /// escaped quotes and an escaped backslash right before its closing quote.
    fn release_json(tag: &str, draft: bool, prerelease: bool) -> String {
        let ver = tag.strip_prefix('v').unwrap_or(tag);
        format!(
            r###"{{
  "url": "https://api.github.com/repos/htsign/loudnorm/releases/1001",
  "upload_url": "https://uploads.github.com/repos/htsign/loudnorm/releases/1001/assets{{?name,label}}",
  "author": {{
    "login": "htsign",
    "url": "https://api.github.com/users/htsign",
    "html_url": "https://github.com/htsign",
    "gists_url": "https://api.github.com/users/htsign/gists{{/gist_id}}",
    "type": "User"
  }},
  "body": "## 変更点\n- \"更新 の確認 ({{ と ] を含む) C:\\",
  "html_url": "https://github.com/htsign/loudnorm/releases/tag/{tag}",
  "id": 1001,
  "tag_name" : "{tag}",
  "target_commitish": "main",
  "name": "loudnorm v0.4.0",
  "draft": {draft},
  "prerelease": {prerelease},
  "assets": [
    {{
      "url": "https://api.github.com/repos/htsign/loudnorm/releases/assets/2001",
      "name": "loudnorm-v{ver}-x86_64-pc-windows-msvc.zip",
      "label": null,
      "uploader": {{ "login": "htsign", "html_url": "https://github.com/htsign" }},
      "size": 12345,
      "browser_download_url": "https://github.com/htsign/loudnorm/releases/download/{tag}/loudnorm-v{ver}-x86_64-pc-windows-msvc.zip"
    }},
    {{
      "url": "https://api.github.com/repos/htsign/loudnorm/releases/assets/2002",
      "name": "SHA256SUMS.txt",
      "label": null,
      "uploader": {{ "login": "htsign", "html_url": "https://github.com/htsign" }},
      "size": 321,
      "browser_download_url": "https://github.com/htsign/loudnorm/releases/download/{tag}/SHA256SUMS.txt"
    }}
  ]
}}"###
        )
    }

    #[test]
    fn parse_accepts_version_with_or_without_v_prefix() {
        assert_eq!(Version::parse("0.3.0"), Some(v(0, 3, 0)));
        assert_eq!(Version::parse("v0.3.0"), Some(v(0, 3, 0)));
    }

    #[test]
    fn parse_rejects_text_that_is_not_three_numbers() {
        for s in ["", "v", "1.2", "1.2.3.4", "a.b.c", "+1.2.3", "1.+2.3", "1.2.+3", "99999999999999999999.0.0"] {
            assert_eq!(Version::parse(s), None, "{s:?}");
        }
    }

    #[test]
    fn parse_accepts_own_package_version() {
        let expected = Version {
            major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
            minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
            patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
        };

        assert_eq!(Version::parse(env!("CARGO_PKG_VERSION")), Some(expected));
    }

    #[test]
    fn parse_rejects_prerelease_versions() {
        assert_eq!(Version::parse("0.4.0-rc.1"), None);
    }

    #[test]
    fn versions_compare_numerically_per_component() {
        assert!(v(0, 10, 0) > v(0, 9, 0));
        assert!(v(1, 0, 0) > v(0, 99, 99));
        assert!(v(0, 9, 99) < v(0, 10, 0));
        assert!(v(0, 3, 1) > v(0, 3, 0));
    }

    #[test]
    fn same_versions_compare_equal() {
        assert_eq!(v(0, 3, 0).cmp(&v(0, 3, 0)), Ordering::Equal);
    }

    #[test]
    fn latest_from_json_takes_the_release_tag_and_page_not_nested_urls() {
        let r = latest_from_json(&release_json("v0.4.0", false, false));

        assert_eq!(
            r.map(|r| (r.version, r.url)),
            Some((v(0, 4, 0), "https://github.com/htsign/loudnorm/releases/tag/v0.4.0".to_string()))
        );
    }

    #[test]
    fn latest_from_json_takes_each_asset_name_and_download_url_not_nested_urls() {
        let r = latest_from_json(&release_json("v0.4.0", false, false));

        assert_eq!(
            r.map(|r| r.assets),
            Some(vec![
                Asset {
                    name: "loudnorm-v0.4.0-x86_64-pc-windows-msvc.zip".into(),
                    url: "https://github.com/htsign/loudnorm/releases/download/v0.4.0/loudnorm-v0.4.0-x86_64-pc-windows-msvc.zip"
                        .into(),
                },
                Asset {
                    name: "SHA256SUMS.txt".into(),
                    url: "https://github.com/htsign/loudnorm/releases/download/v0.4.0/SHA256SUMS.txt".into(),
                },
            ])
        );
    }

    #[test]
    fn latest_from_json_skips_only_the_assets_without_a_name_or_a_download_url() {
        let mut json: serde_json::Value = serde_json::from_str(&release_json("v0.4.0", false, false)).unwrap();
        json["assets"] = serde_json::json!([
            { "browser_download_url": "https://github.com/htsign/loudnorm/releases/download/v0.4.0/no-name.zip" },
            { "name": "no-url.zip" },
            { "name": "SHA256SUMS.txt", "browser_download_url": "https://github.com/htsign/loudnorm/releases/download/v0.4.0/SHA256SUMS.txt" },
        ]);

        let r = latest_from_json(&json.to_string());

        assert_eq!(
            r.map(|r| r.assets),
            Some(vec![Asset {
                name: "SHA256SUMS.txt".into(),
                url: "https://github.com/htsign/loudnorm/releases/download/v0.4.0/SHA256SUMS.txt".into(),
            }])
        );
    }

    #[test]
    fn latest_from_json_has_no_assets_when_assets_is_null() {
        let mut json: serde_json::Value = serde_json::from_str(&release_json("v0.4.0", false, false)).unwrap();
        json["assets"] = serde_json::Value::Null;

        let r = latest_from_json(&json.to_string());

        assert_eq!(r.map(|r| r.assets), Some(Vec::new()));
    }

    #[test]
    fn latest_from_json_has_no_assets_when_the_response_has_no_assets_key() {
        let mut json: serde_json::Value = serde_json::from_str(&release_json("v0.4.0", false, false)).unwrap();
        json.as_object_mut().unwrap().remove("assets");

        let r = latest_from_json(&json.to_string());

        assert_eq!(r.map(|r| r.assets), Some(Vec::new()));
    }

    #[test]
    fn latest_from_json_ignores_prereleases() {
        assert_eq!(latest_from_json(&release_json("v0.4.0", false, true)), None);
    }

    #[test]
    fn latest_from_json_ignores_drafts() {
        assert_eq!(latest_from_json(&release_json("v0.4.0", true, false)), None);
    }

    #[test]
    fn latest_from_json_returns_none_without_a_release_tag() {
        let not_found = r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest"}"#;

        assert_eq!(latest_from_json(not_found), None);
    }

    #[test]
    fn latest_from_json_returns_none_when_tag_is_not_a_version() {
        assert_eq!(latest_from_json(&release_json("nightly", false, false)), None);
    }

    #[test]
    fn latest_from_json_rejects_malformed_json() {
        let truncated = r#"{"html_url": "https://github.com/htsign/loudnorm/releases/tag/v0.4.0", "tag_name": "v0.4.0", "draft": false, "prerelease": false"#;

        assert_eq!(latest_from_json(truncated), None);
    }

    #[test]
    fn is_newer_only_when_latest_is_greater() {
        let current = v(0, 3, 0);

        assert!(is_newer(&v(0, 3, 1), &current), "newer");
        assert!(!is_newer(&v(0, 3, 0), &current), "equal");
        assert!(!is_newer(&v(0, 2, 9), &current), "older");
    }

    /// `release_json` for `v0.4.0` with the release's own `html_url` replaced by `page`.
    fn release_json_with_page(page: &str) -> String {
        let mut json: serde_json::Value = serde_json::from_str(&release_json("v0.4.0", false, false)).unwrap();
        json["html_url"] = page.into();
        json.to_string()
    }

    #[test]
    fn latest_from_json_returns_none_when_page_is_not_a_release_of_this_repository() {
        for page in [
            "https://example.com/htsign/loudnorm/releases/tag/v0.4.0",
            "https://github.com/evil/loudnorm/releases/tag/v0.4.0",
            "https://github.com/htsign/loudnorm-evil/releases/tag/v0.4.0",
            "http://github.com/htsign/loudnorm/releases/tag/v0.4.0",
            "https://github.com.evil.example/htsign/loudnorm/releases/tag/v0.4.0",
            "https://github.com@evil.example/htsign/loudnorm/releases/tag/v0.4.0",
            "https://github.com/htsign/loudnorm/releases/../../evil/loudnorm/releases/tag/v0.4.0",
            "https://github.com/htsign/loudnorm/releases/%2e%2e/%2e%2e/evil/loudnorm/releases/tag/v0.4.0",
            r"https://github.com/htsign/loudnorm/releases/..\..\evil/loudnorm/releases/tag/v0.4.0",
            "https://github.com/htsign/loudnorm/releases/tag/v0.4.0?x=1",
            "https://github.com/htsign/loudnorm/releases/tag/v0.4.0#x",
            "https://github.com/htsign/loudnorm/releases/",
            "https://github.com/htsign/loudnorm/releases//tag/v0.4.0",
            "https://github.com/htsign/loudnorm/releases/./tag/v0.4.0",
            "https://github.com/htsign/loudnorm/releases/tag/v0.4.0/",
        ] {
            let r = latest_from_json(&release_json_with_page(page));

            assert_eq!(r, None, "{page}");
        }
    }

    fn release(major: u64, minor: u64, patch: u64) -> Release {
        Release {
            version: v(major, minor, patch),
            url: format!("https://github.com/htsign/loudnorm/releases/tag/v{major}.{minor}.{patch}"),
            assets: Vec::new(),
        }
    }

    #[test]
    fn next_outcome_is_available_when_the_fetched_release_is_newer() {
        let current = v(0, 4, 0);

        let next = next_outcome(&Outcome::NotChecked, &current, Ok(release(0, 5, 0)));

        assert_eq!(next, Outcome::Available(release(0, 5, 0)));
    }

    #[test]
    fn next_outcome_replaces_a_known_release_with_a_newer_one() {
        let current = v(0, 4, 0);
        let prev = Outcome::Available(release(0, 5, 0));

        let next = next_outcome(&prev, &current, Ok(release(0, 6, 0)));

        assert_eq!(next, Outcome::Available(release(0, 6, 0)));
    }

    #[test]
    fn next_outcome_is_up_to_date_when_the_fetched_release_is_the_current_version() {
        let current = v(0, 4, 0);

        let next = next_outcome(&Outcome::NotChecked, &current, Ok(release(0, 4, 0)));

        assert_eq!(next, Outcome::UpToDate);
    }

    #[test]
    fn next_outcome_is_up_to_date_when_the_fetched_release_is_older() {
        let current = v(0, 4, 0);

        let next = next_outcome(&Outcome::NotChecked, &current, Ok(release(0, 3, 9)));

        assert_eq!(next, Outcome::UpToDate);
    }

    #[test]
    fn next_outcome_is_up_to_date_when_a_known_release_is_no_longer_newer() {
        let current = v(0, 4, 0);
        let prev = Outcome::Available(release(0, 5, 0));

        let next = next_outcome(&prev, &current, Ok(release(0, 4, 0)));

        assert_eq!(next, Outcome::UpToDate);
    }

    #[test]
    fn next_outcome_is_available_when_a_recheck_succeeds_after_a_failure() {
        let current = v(0, 4, 0);
        let prev = Outcome::Failed("通信に失敗しました".into());

        let next = next_outcome(&prev, &current, Ok(release(0, 5, 0)));

        assert_eq!(next, Outcome::Available(release(0, 5, 0)));
    }

    #[test]
    fn next_outcome_keeps_the_known_release_when_a_recheck_fails() {
        let current = v(0, 4, 0);
        let prev = Outcome::Available(release(0, 5, 0));

        let next = next_outcome(&prev, &current, Err("通信に失敗しました".into()));

        assert_eq!(next, Outcome::Available(release(0, 5, 0)));
    }

    #[test]
    fn next_outcome_is_failed_with_the_message_when_no_release_is_known() {
        let current = v(0, 4, 0);

        for prev in [Outcome::NotChecked, Outcome::UpToDate, Outcome::Failed("前回の失敗".into())] {
            let next = next_outcome(&prev, &current, Err("通信に失敗しました".into()));

            assert_eq!(next, Outcome::Failed("通信に失敗しました".into()), "{prev:?}");
        }
    }

    #[test]
    fn asset_name_is_a_zip_for_windows_targets() {
        for target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
            let name = asset_name(&v(0, 5, 0), target);

            assert_eq!(name, Some(format!("loudnorm-v0.5.0-{target}.zip")), "{target}");
        }
    }

    #[test]
    fn asset_name_is_a_zip_for_macos_targets() {
        for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
            let name = asset_name(&v(0, 5, 0), target);

            assert_eq!(name, Some(format!("loudnorm-v0.5.0-{target}.zip")), "{target}");
        }
    }

    #[test]
    fn asset_name_is_a_tar_gz_for_linux_targets() {
        for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
            let name = asset_name(&v(0, 5, 0), target);

            assert_eq!(name, Some(format!("loudnorm-v0.5.0-{target}.tar.gz")), "{target}");
        }
    }

    #[test]
    fn asset_name_is_none_for_a_target_that_is_not_windows_msvc_macos_or_linux_gnu() {
        let name = asset_name(&v(0, 5, 0), "wasm32-unknown-unknown");

        assert_eq!(name, None);
    }

    const WINDOWS_ZIP: &str = "loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip";

    /// `release(0, 5, 0)` with the given `(name, url)` assets.
    fn release_with_assets(assets: &[(&str, &str)]) -> Release {
        Release {
            assets: assets.iter().map(|&(name, url)| Asset { name: name.into(), url: url.into() }).collect(),
            ..release(0, 5, 0)
        }
    }

    #[test]
    fn download_url_returns_the_url_of_the_asset_with_the_name() {
        let release = release_with_assets(&[
            (
                "loudnorm-v0.5.0-aarch64-pc-windows-msvc.zip",
                "https://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-aarch64-pc-windows-msvc.zip",
            ),
            (WINDOWS_ZIP, "https://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip"),
        ]);

        let url = download_url(&release, WINDOWS_ZIP);

        assert_eq!(url, Some("https://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip"));
    }

    #[test]
    fn download_url_is_none_when_no_asset_has_exactly_the_name() {
        let release = release_with_assets(&[(
            WINDOWS_ZIP,
            "https://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
        )]);

        for name in [
            "loudnorm-v0.5.0-aarch64-pc-windows-msvc.zip",
            "loudnorm-v0.5.0-x86_64-pc-windows-msvc",
            "loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip.bak",
        ] {
            let url = download_url(&release, name);

            assert_eq!(url, None, "{name}");
        }
    }

    #[test]
    fn download_url_is_none_when_the_url_is_not_a_download_from_this_repository() {
        for asset_url in [
            "https://example.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/evil/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/htsign/loudnorm-evil/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "http://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/htsign/loudnorm/releases/download/../../../evil/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/htsign/loudnorm/releases/download/%2e%2e/%2e%2e/%2e%2e/evil/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/htsign/loudnorm/releases/download//v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip",
            "https://github.com/htsign/loudnorm/releases/download/v0.5.0/loudnorm-v0.5.0-x86_64-pc-windows-msvc.zip?x=1",
            "https://github.com/htsign/loudnorm/releases/tag/v0.5.0",
        ] {
            let release = release_with_assets(&[(WINDOWS_ZIP, asset_url)]);

            let url = download_url(&release, WINDOWS_ZIP);

            assert_eq!(url, None, "{asset_url}");
        }
    }

    #[test]
    fn download_url_is_none_when_the_url_points_at_another_tag_of_this_repository() {
        let release = release_with_assets(&[(
            WINDOWS_ZIP,
            "https://github.com/htsign/loudnorm/releases/download/v0.1.0/loudnorm-v0.1.0-x86_64-pc-windows-msvc.zip",
        )]);

        let url = download_url(&release, WINDOWS_ZIP);

        assert_eq!(url, None);
    }

    #[test]
    fn download_url_is_none_when_the_last_part_of_the_url_is_not_the_asset_name() {
        let release = release_with_assets(&[(WINDOWS_ZIP, "https://github.com/htsign/loudnorm/releases/download/v0.5.0/other.zip")]);

        let url = download_url(&release, WINDOWS_ZIP);

        assert_eq!(url, None);
    }

    const HASH_A_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const HASH_A: [u8; 32] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, //
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    ];
    const HASH_B_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[test]
    fn sha256_from_sums_reads_the_hash_on_the_line_of_the_file() {
        let sums = format!("{HASH_B_HEX}  loudnorm-v0.5.0-src.zip\n{HASH_A_HEX}  {WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }

    #[test]
    fn sha256_from_sums_reads_a_line_in_binary_mode() {
        let sums = format!("{HASH_A_HEX} *{WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }

    #[test]
    fn sha256_from_sums_skips_a_line_with_a_single_space_before_the_name() {
        let sums = format!("{HASH_B_HEX} {WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, None);
    }

    #[test]
    fn sha256_from_sums_skips_a_line_with_three_spaces_before_the_name() {
        let sums = format!("{HASH_B_HEX}   {WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, None);
    }

    #[test]
    fn sha256_from_sums_does_not_read_a_text_mode_name_starting_with_an_asterisk_as_the_file() {
        let sums = format!("{HASH_B_HEX}  *{WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, None);
    }

    #[test]
    fn sha256_from_sums_skips_a_line_with_non_ascii_text_around_the_hash_position() {
        let sums = format!("{}あ  {WINDOWS_ZIP}\n{HASH_A_HEX}  {WINDOWS_ZIP}\n", &HASH_B_HEX[..62]);

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }

    #[test]
    fn sha256_from_sums_reads_uppercase_hex() {
        let sums = format!("0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF  {WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }

    #[test]
    fn sha256_from_sums_skips_lines_whose_name_only_starts_with_the_file() {
        let sums = format!("{HASH_B_HEX}  {WINDOWS_ZIP}.bak\n{HASH_A_HEX}  {WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }

    #[test]
    fn sha256_from_sums_is_none_when_only_other_names_contain_the_file() {
        let sums = format!("{HASH_B_HEX}  {WINDOWS_ZIP}.bak\n{HASH_B_HEX}  old-{WINDOWS_ZIP}\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, None);
    }

    #[test]
    fn sha256_from_sums_ignores_lines_without_64_hex_digits() {
        for bad in [
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
        ] {
            let only_bad = format!("{bad}  {WINDOWS_ZIP}\n");
            let bad_then_good = format!("{bad}  {WINDOWS_ZIP}\n{HASH_A_HEX}  {WINDOWS_ZIP}\n");

            let from_only_bad = sha256_from_sums(&only_bad, WINDOWS_ZIP);
            let from_bad_then_good = sha256_from_sums(&bad_then_good, WINDOWS_ZIP);

            assert_eq!(from_only_bad, None, "{bad}");
            assert_eq!(from_bad_then_good, Some(HASH_A), "{bad}");
        }
    }

    #[test]
    fn sha256_from_sums_is_none_when_the_file_is_not_listed() {
        for sums in ["", &format!("{HASH_B_HEX}  loudnorm-v0.5.0-src.zip\n")] {
            let hash = sha256_from_sums(sums, WINDOWS_ZIP);

            assert_eq!(hash, None, "{sums:?}");
        }
    }

    #[test]
    fn sha256_from_sums_reads_lines_ending_with_crlf() {
        let sums = format!("{HASH_A_HEX}  {WINDOWS_ZIP}\r\n{HASH_B_HEX}  loudnorm-v0.5.0-src.zip\r\n");

        let hash = sha256_from_sums(&sums, WINDOWS_ZIP);

        assert_eq!(hash, Some(HASH_A));
    }
}

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

/// The latest published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version the release's tag names.
    pub version: Version,
    /// The release's page on GitHub (`html_url`).
    pub url: String,
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
    Some(Release { version: Version::parse(&r.tag_name?)?, url })
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
            r,
            Some(Release { version: v(0, 4, 0), url: "https://github.com/htsign/loudnorm/releases/tag/v0.4.0".into() })
        );
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
}

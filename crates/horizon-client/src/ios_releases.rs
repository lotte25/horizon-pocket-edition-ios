//! iOS: where official Starling releases come out, and which of them is an update of this app:
//! for the update prompt (game/update_offer.rs).
//!
//! Official releases come only from `lotte25/horizon-pocket-edition-ios` with tags in `x.x.x`
//! format (e.g. `1.0.0`, `1.2.3`) and an iOS asset named `Starling.ipa`. Drafts, pre-releases,
//! and releases with other tag formats or asset names are ignored.
use crate::starling_releases::{Update, newer, release_version};

/// The repository the official releases come out in, as GitHub's API names it.
pub const REPOSITORY: &str = "lotte25/horizon-pocket-edition-ios";

/// The iOS release asset file name.
pub const ASSET: &str = "Starling.ipa";

/// The app name shown in the update prompt.
pub const APP_NAME: &str = "Starling";

/// The version of an official release tag (`x.x.x`, e.g. `1.0.0`).
pub fn version_of(tag: &str) -> Option<&str> {
    release_version(tag)
}

/// The repository's releases, newest first, as GitHub's API serves them.
pub fn releases_url() -> String {
    format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=50")
}

/// The newest official release in `releases` (the API's list) that comes after `current` and
/// carries `Starling.ipa`. Drafts and pre-releases are never updates.
pub fn update_in(releases: &serde_json::Value, current: &str) -> Option<Update> {
    let mut best: Option<Update> = None;
    for release in releases.as_array()? {
        if release["draft"].as_bool() != Some(false)
            || release["prerelease"].as_bool() != Some(false)
        {
            continue;
        }
        let Some(version) = release["tag_name"].as_str().and_then(version_of) else {
            continue;
        };
        if !newer(version, current) || best.as_ref().is_some_and(|b| !newer(version, &b.version)) {
            continue;
        }
        let Some(ipa) = release["assets"]
            .as_array()
            .and_then(|assets| assets.iter().find(|a| a["name"].as_str() == Some(ASSET)))
        else {
            continue;
        };
        let (Some(name), Some(url)) = (
            ipa["name"].as_str(),
            ipa["browser_download_url"]
                .as_str()
                .filter(|u| u.starts_with("https://")),
        ) else {
            continue;
        };
        best = Some(Update {
            version: version.to_owned(),
            name: name.to_owned(),
            url: url.to_owned(),
            size: ipa["size"].as_u64().unwrap_or(0),
            sha256: ipa["digest"]
                .as_str()
                .and_then(|d| d.strip_prefix("sha256:"))
                .map(str::to_owned),
        });
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(tag: &str, asset: &str) -> serde_json::Value {
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "Starling.apk", "browser_download_url": format!("https://example.test/{tag}/Starling.apk"), "size": 9},
                {
                    "name": asset,
                    "browser_download_url": format!("https://example.test/{tag}/{asset}"),
                    "size": 123_456_789u64,
                    "digest": "sha256:abc123",
                },
            ],
        })
    }

    #[test]
    fn repository_points_to_official_fork() {
        assert_eq!(REPOSITORY, "lotte25/horizon-pocket-edition-ios");
        assert_eq!(ASSET, "Starling.ipa");
    }

    #[test]
    fn release_tags_must_match_x_x_x() {
        assert_eq!(version_of("1.0.0"), Some("1.0.0"));
        assert_eq!(version_of("0.9.12"), Some("0.9.12"));
        assert_eq!(version_of("2.10.3"), Some("2.10.3"));
        for odd in [
            "v1.0.0",
            "v1.0.0-shared-worlds",
            "v1.0.0-shared-worlds-mods",
            "1.0",
            "1.0.0.0",
            "1..0",
            "1.0.0-beta",
            "native-builds",
            "1.0.x",
            "",
        ] {
            assert_eq!(version_of(odd), None, "{odd}");
        }
    }

    #[test]
    fn newest_official_release_with_starling_ipa_is_offered() {
        let releases = json!([
            release("v1.5.0-shared-worlds-mods", "Starling.ipa"),
            release("1.2.0", "HorizonPE-1.2.0.ipa"),
            release("1.1.0", "Starling.ipa"),
            release("1.0.1", "Starling.ipa"),
            release("1.0.0", "Starling.ipa"),
        ]);
        let update = update_in(&releases, "1.0.0").unwrap();
        assert_eq!(update.version, "1.1.0");
        assert_eq!(update.name, "Starling.ipa");
        assert_eq!(update.url, "https://example.test/1.1.0/Starling.ipa");
        assert_eq!(update.size, 123_456_789);
        assert_eq!(update.sha256.as_deref(), Some("abc123"));
        // Up to date or ahead: nothing offered.
        assert_eq!(update_in(&releases, "1.1.0"), None);
        assert_eq!(update_in(&releases, "2.0.0"), None);
        // 1.10.0 comes after 1.9.0, number by number.
        let releases = json!([
            release("1.9.0", "Starling.ipa"),
            release("1.10.0", "Starling.ipa"),
        ]);
        assert_eq!(update_in(&releases, "1.0.0").unwrap().version, "1.10.0");
    }

    #[test]
    fn drafts_prereleases_and_releases_without_starling_ipa_are_not_updates() {
        let mut draft = release("3.0.0", "Starling.ipa");
        draft["draft"] = json!(true);
        let mut pre = release("2.5.0", "Starling.ipa");
        pre["prerelease"] = json!(true);
        let mut bare = release("2.0.0", "Starling.ipa");
        bare["assets"] = json!([{"name": "Starling.apk", "browser_download_url": "https://example.test/Starling.apk"}]);
        let mut plain_http = release("1.5.0", "Starling.ipa");
        plain_http["assets"][1]["browser_download_url"] = json!("http://example.test/Starling.ipa");
        let releases = json!([
            draft,
            pre,
            bare,
            plain_http,
            release("1.1.0", "Starling.ipa")
        ]);
        assert_eq!(update_in(&releases, "1.0.0").unwrap().version, "1.1.0");
        // Not a list (an error's body), or an empty one: nothing.
        assert_eq!(
            update_in(&json!({"message": "rate limited"}), "1.0.0"),
            None
        );
        assert_eq!(update_in(&json!([]), "1.0.0"), None);
    }
}

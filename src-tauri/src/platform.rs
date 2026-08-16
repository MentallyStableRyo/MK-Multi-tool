//! Which service a link came from, and how to canonicalise it.
//!
//! Every tab in the UI is bound to one [`Platform`], and every request carries
//! the tab it came from. Normalisation is therefore per-platform and
//! deliberately strict: a TikTok link pasted into the Instagram tab is rejected
//! here — with a message naming the tab — instead of becoming an opaque yt-dlp
//! error three seconds later. It is also the choke point that stops arbitrary
//! URLs (`file://`, unrelated hosts) from reaching the sidecar at all.
//!
//! Only the shapes that identify a *single piece of media* are accepted.
//! Profiles, feeds, and playlists have no timeline to slice.

use serde::Deserialize;

use crate::error::{AppError, Result};
use crate::youtube;

/// A supported source site. Serialised as `"youtube"`, `"instagram"`, … to
/// match the tab ids in `src/platforms.ts`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Platform {
    #[default]
    Youtube,
    Instagram,
    Tiktok,
    Twitter,
}

impl Platform {
    /// Display name, used in every message the user sees.
    pub fn label(self) -> &'static str {
        match self {
            Self::Youtube => "YouTube",
            Self::Instagram => "Instagram",
            Self::Tiktok => "TikTok",
            Self::Twitter => "X",
        }
    }

    /// The link shapes this platform accepts, quoted back when one is rejected.
    pub fn accepted(self) -> &'static str {
        match self {
            Self::Youtube => "youtube.com/watch?v=…, youtu.be/…, /shorts/…, /live/…, /embed/…",
            Self::Instagram => "instagram.com/p/…, /reel/…, /tv/…, or a share link",
            Self::Tiktok => "tiktok.com/@user/video/…, vm.tiktok.com/…, vt.tiktok.com/…",
            Self::Twitter => "x.com/user/status/… or twitter.com/user/status/…",
        }
    }

    /// Canonicalise a pasted link, or explain why it cannot be used.
    pub fn normalize(self, raw: &str) -> Result<String> {
        match self {
            Self::Youtube => youtube::normalize_url(raw),
            Self::Instagram => instagram(raw),
            Self::Tiktok => tiktok(raw),
            Self::Twitter => twitter(raw),
        }
    }

    /// The rejection this platform produces for an unusable link.
    pub fn reject(self, url: &str) -> AppError {
        AppError::invalid_url(self.label(), self.accepted(), url)
    }
}

// ---------------------------------------------------------------------------
// Shared parsing helpers
// ---------------------------------------------------------------------------

/// Parse into a plain web URL, tolerating a missing scheme (the common paste).
fn web_url(platform: Platform, raw: &str) -> Result<url::Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(platform.reject("(empty)"));
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let parsed = url::Url::parse(&with_scheme).map_err(|_| platform.reject(trimmed))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(platform.reject(trimmed));
    }
    Ok(parsed)
}

/// Host without the interchangeable `www.` / `m.` / `mobile.` prefixes.
fn host_of(u: &url::Url) -> String {
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    for prefix in ["www.", "m.", "mobile."] {
        if let Some(rest) = host.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    host
}

fn segments(u: &url::Url) -> Vec<&str> {
    u.path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect()
}

/// The opaque short codes these sites use for posts: base62 plus `-` and `_`.
fn is_shortcode(s: &str) -> bool {
    (4..=48).contains(&s.chars().count())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A numeric post id. Bounded so a stray path segment cannot pass as one.
fn is_numeric_id(s: &str) -> bool {
    (5..=25).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Instagram
// ---------------------------------------------------------------------------

/// Post kinds that carry media. `reels` is the plural form the app links to.
const INSTAGRAM_KINDS: &[&str] = &["p", "reel", "reels", "tv"];

/// `/p/<code>/`, `/reel/<code>/`, `/tv/<code>/`, optionally prefixed with the
/// author's handle (`/<user>/reel/<code>/`), plus opaque `/share/…` links.
fn instagram(raw: &str) -> Result<String> {
    let platform = Platform::Instagram;
    let parsed = web_url(platform, raw)?;

    if host_of(&parsed) != "instagram.com" {
        return Err(platform.reject(raw.trim()));
    }

    let segs = segments(&parsed);

    // Share links carry a token that only resolves through Instagram's own
    // redirect, so there is nothing to canonicalise — hand yt-dlp the path.
    if segs.first() == Some(&"share") {
        let rest: Vec<&str> = segs.iter().skip(1).copied().collect();
        if rest.is_empty() || !rest.iter().all(|s| is_shortcode(s)) {
            return Err(platform.reject(raw.trim()));
        }
        return Ok(format!("https://www.instagram.com/share/{}/", rest.join("/")));
    }

    let at = segs
        .iter()
        .position(|s| INSTAGRAM_KINDS.contains(s))
        .ok_or_else(|| platform.reject(raw.trim()))?;

    let kind = match segs[at] {
        "reels" => "reel",
        other => other,
    };
    let code = segs
        .get(at + 1)
        .filter(|c| is_shortcode(c))
        .ok_or_else(|| platform.reject(raw.trim()))?;

    Ok(format!("https://www.instagram.com/{kind}/{code}/"))
}

// ---------------------------------------------------------------------------
// TikTok
// ---------------------------------------------------------------------------

/// `/@user/video/<id>`, plus the `vm.`/`vt.`/`/t/` short links, which are left
/// intact because only TikTok's redirect knows what they point at.
fn tiktok(raw: &str) -> Result<String> {
    let platform = Platform::Tiktok;
    let parsed = web_url(platform, raw)?;
    let host = host_of(&parsed);
    let segs = segments(&parsed);
    let reject = || platform.reject(raw.trim());

    match host.as_str() {
        "vm.tiktok.com" | "vt.tiktok.com" => {
            let code = segs.first().filter(|c| is_shortcode(c)).ok_or_else(reject)?;
            Ok(format!("https://{host}/{code}"))
        }

        "tiktok.com" => {
            if segs.first() == Some(&"t") {
                let code = segs.get(1).filter(|c| is_shortcode(c)).ok_or_else(reject)?;
                return Ok(format!("https://www.tiktok.com/t/{code}/"));
            }

            let user = segs
                .first()
                .filter(|s| s.starts_with('@') && s.len() > 1)
                .ok_or_else(reject)?;

            // Photo posts are slideshows, not video — there is no range to cut.
            if segs.get(1) != Some(&"video") {
                return Err(reject());
            }

            let id = segs.get(2).filter(|id| is_numeric_id(id)).ok_or_else(reject)?;
            Ok(format!("https://www.tiktok.com/{user}/video/{id}"))
        }

        _ => Err(reject()),
    }
}

// ---------------------------------------------------------------------------
// X (Twitter)
// ---------------------------------------------------------------------------

/// `/<user>/status/<id>`, in any of the shapes X still serves. The canonical
/// form is always `x.com`; `twitter.com` merely redirects to it now.
fn twitter(raw: &str) -> Result<String> {
    let platform = Platform::Twitter;
    let parsed = web_url(platform, raw)?;

    if !matches!(host_of(&parsed).as_str(), "twitter.com" | "x.com") {
        return Err(platform.reject(raw.trim()));
    }

    let segs = segments(&parsed);
    let at = segs
        .iter()
        .position(|s| *s == "status" || *s == "statuses")
        .ok_or_else(|| platform.reject(raw.trim()))?;

    let id = segs
        .get(at + 1)
        .filter(|id| is_numeric_id(id))
        .ok_or_else(|| platform.reject(raw.trim()))?;

    // `/i/web/status/<id>` and bare `/status/<id>` have no usable handle in
    // them; `i` is X's own placeholder and resolves for any public post, so it
    // also stands in for the `web` segment rather than being mistaken for one.
    let user = at
        .checked_sub(1)
        .and_then(|i| segs.get(i))
        .filter(|u| is_handle(u) && **u != "web")
        .copied()
        .unwrap_or("i");

    Ok(format!("https://x.com/{user}/status/{id}"))
}

fn is_handle(s: &str) -> bool {
    (1..=15).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_instagram_link_shapes() {
        for (input, expected) in [
            (
                "https://www.instagram.com/p/CabcDEF1234/",
                "https://www.instagram.com/p/CabcDEF1234/",
            ),
            (
                "https://instagram.com/reel/CabcDEF1234/?igshid=xyz",
                "https://www.instagram.com/reel/CabcDEF1234/",
            ),
            (
                "https://www.instagram.com/reels/CabcDEF1234",
                "https://www.instagram.com/reel/CabcDEF1234/",
            ),
            (
                "https://www.instagram.com/someuser/reel/CabcDEF1234/",
                "https://www.instagram.com/reel/CabcDEF1234/",
            ),
            (
                "instagram.com/tv/CabcDEF1234/",
                "https://www.instagram.com/tv/CabcDEF1234/",
            ),
            (
                "https://www.instagram.com/share/reel/AbCdEf123/",
                "https://www.instagram.com/share/reel/AbCdEf123/",
            ),
        ] {
            assert_eq!(
                Platform::Instagram.normalize(input).unwrap(),
                expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn normalises_tiktok_link_shapes() {
        for (input, expected) in [
            (
                "https://www.tiktok.com/@someone/video/7212345678901234567?is_from_webapp=1",
                "https://www.tiktok.com/@someone/video/7212345678901234567",
            ),
            (
                "tiktok.com/@someone/video/7212345678901234567",
                "https://www.tiktok.com/@someone/video/7212345678901234567",
            ),
            ("https://vm.tiktok.com/ZMabcdef1/", "https://vm.tiktok.com/ZMabcdef1"),
            ("https://vt.tiktok.com/ZSabcdef1/", "https://vt.tiktok.com/ZSabcdef1"),
            ("https://www.tiktok.com/t/ZTabcdef1/", "https://www.tiktok.com/t/ZTabcdef1/"),
        ] {
            assert_eq!(
                Platform::Tiktok.normalize(input).unwrap(),
                expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn normalises_x_link_shapes() {
        let expected = "https://x.com/someone/status/1234567890123456789";
        for input in [
            "https://x.com/someone/status/1234567890123456789",
            "https://twitter.com/someone/status/1234567890123456789?s=20&t=abc",
            "https://mobile.twitter.com/someone/status/1234567890123456789",
            "https://x.com/someone/status/1234567890123456789/video/1",
            "twitter.com/someone/statuses/1234567890123456789",
        ] {
            assert_eq!(Platform::Twitter.normalize(input).unwrap(), expected, "input: {input}");
        }

        // No usable handle in the path — X's own placeholder stands in.
        assert_eq!(
            Platform::Twitter
                .normalize("https://twitter.com/i/web/status/1234567890123456789")
                .unwrap(),
            "https://x.com/i/status/1234567890123456789"
        );
    }

    #[test]
    fn rejects_profiles_feeds_and_the_wrong_platform() {
        for (platform, input) in [
            (Platform::Instagram, "https://www.instagram.com/someuser/"),
            (Platform::Instagram, "https://www.instagram.com/p/"),
            (Platform::Instagram, "https://www.tiktok.com/@a/video/7212345678901"),
            (Platform::Tiktok, "https://www.tiktok.com/@someone"),
            (Platform::Tiktok, "https://www.tiktok.com/@someone/photo/7212345678901"),
            (Platform::Tiktok, "https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            (Platform::Twitter, "https://x.com/someone"),
            (Platform::Twitter, "https://x.com/someone/status/abc"),
            (Platform::Twitter, "https://evil.com/someone/status/1234567890123"),
            (Platform::Youtube, "https://www.instagram.com/p/CabcDEF1234/"),
            (Platform::Instagram, "file:///etc/passwd"),
            (Platform::Tiktok, ""),
        ] {
            assert!(
                platform.normalize(input).is_err(),
                "{:?} should reject: {input}",
                platform
            );
        }
    }

    #[test]
    fn rejection_names_the_tab_the_link_was_pasted_into() {
        let error = Platform::Tiktok.normalize("https://x.com/a/status/1234567890123").unwrap_err();
        assert_eq!(error.kind(), "invalid_url");
        assert!(error.to_string().contains("TikTok"));
        assert!(error.hint().unwrap().contains("vm.tiktok.com"));
    }
}

//! Typed error surface for the whole backend.
//!
//! Every fallible path returns [`Result`]. Nothing in this crate panics on
//! recoverable conditions: `unwrap`/`expect` are reserved for invariants that
//! cannot fail (e.g. static regex-free parsing of our own output format).
//!
//! Errors serialize into a stable `{ kind, message, hint }` shape so the
//! frontend can render an actionable message instead of a stack trace.

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, AppError>;

/// Why a metadata fetch failed. Derived by pattern-matching yt-dlp's stderr,
/// because yt-dlp does not expose machine-readable failure codes.
///
/// The wording of the hints is kept site-neutral: every platform in
/// [`crate::platform`] goes through the same extractor, and the remedy —
/// usually "lend it your browser session" — is the same everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YtErrorKind {
    Private,
    BotCheck,
    LoginRequired,
    AgeRestricted,
    MembersOnly,
    GeoBlocked,
    Removed,
    LiveStream,
    NotFound,
    Network,
    RateLimited,
    Unknown,
}

impl YtErrorKind {
    /// Classify a yt-dlp stderr blob. Order matters: more specific patterns first.
    pub fn classify(stderr: &str) -> Self {
        let s = stderr.to_ascii_lowercase();
        let has = |needle: &str| s.contains(needle);

        if has("private video")
            || has("this video is private")
            || has("this account is private")
            || has("private account")
        {
            Self::Private
        } else if has("unexpected response from webpage request")
            || has("captcha")
            || has("verify you are human")
            || has("sign in to confirm you're not a bot")
            || has("sign in to confirm you are not a bot")
        {
            // The site served a challenge page instead of the media. TikTok
            // reports it as an "unexpected response" and points at the yt-dlp
            // issue tracker, which is a dead end for the user — the extractor
            // is fine, the request was blocked.
            Self::BotCheck
        } else if has("login required")
            || has("requested content is not available")
            || has("requires authentication")
            || has("use --cookies")
            || has("cookies-from-browser")
        {
            // Instagram and X answer nearly every unauthenticated request this
            // way; cookies are the fix rather than anything about the post.
            Self::LoginRequired
        } else if has("confirm your age")
            || has("age-restricted")
            || has("age restricted")
            || has("sign in to confirm your age")
            || has("inappropriate for some users")
        {
            Self::AgeRestricted
        } else if has("members-only") || has("join this channel") || has("members only") {
            Self::MembersOnly
        } else if has("not available in your country")
            || has("blocked it in your country")
            || has("geo restriction")
            || has("geo-restricted")
        {
            Self::GeoBlocked
        } else if has("has been removed")
            || has("account associated with this video has been terminated")
            || has("video unavailable")
            || has("removed by the uploader")
            || has("post has been deleted")
            || has("tweet is unavailable")
        {
            Self::Removed
        } else if has("is live") || has("live event will begin") || has("premieres in") {
            Self::LiveStream
        } else if has("incomplete youtube id")
            || has("unsupported url")
            || has("no video could be found")
            || has("there's no video")
            || has("404")
        {
            Self::NotFound
        } else if has("http error 429") || has("too many requests") || has("rate-limit reached") {
            Self::RateLimited
        } else if has("unable to download")
            || has("connection reset")
            || has("timed out")
            || has("temporary failure in name resolution")
            || has("network is unreachable")
            || has("getaddrinfo")
        {
            Self::Network
        } else if has("sign in to confirm") || has("bot") && has("sign in") {
            // YouTube's anti-automation interstitial, worded differently again.
            Self::BotCheck
        } else {
            Self::Unknown
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::BotCheck => "bot_check",
            Self::LoginRequired => "login_required",
            Self::AgeRestricted => "age_restricted",
            Self::MembersOnly => "members_only",
            Self::GeoBlocked => "geo_blocked",
            Self::Removed => "removed",
            Self::LiveStream => "live_stream",
            Self::NotFound => "not_found",
            Self::Network => "network",
            Self::RateLimited => "rate_limited",
            Self::Unknown => "unknown",
        }
    }

    /// A concrete next step the user can actually take.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::Private => Some(
                "This post is private. If you have access, open Advanced and pick the browser \
                 you're signed in with.",
            ),
            Self::BotCheck => Some(
                "The site is challenging automated requests from this network — it served a \
                 verification page instead of the video. Open Advanced and pick a browser you \
                 have the site open in; borrowing that session usually clears the check. \
                 Failing that, a different network or a VPN will.",
            ),
            Self::LoginRequired => Some(
                "This site will only serve the media to a signed-in session. Open Advanced and \
                 pick a browser you're logged into — nothing is uploaded, the cookies are read \
                 locally.",
            ),
            Self::AgeRestricted => Some(
                "The site wants a signed-in session. Open Advanced, choose \"Browser cookies\", \
                 and select a browser where you're logged in.",
            ),
            Self::MembersOnly => Some(
                "Members-only video. Choose browser cookies from an account that holds the \
                 channel membership.",
            ),
            Self::GeoBlocked => Some(
                "Blocked in your region. A VPN or proxy in an allowed region is the only fix; \
                 the app cannot bypass this.",
            ),
            Self::Removed => Some("The post no longer exists, or was made unavailable."),
            Self::LiveStream => Some(
                "Live and upcoming streams can't be sliced. Wait until the VOD is published.",
            ),
            Self::NotFound => Some("Double-check the link — no video could be resolved from it."),
            Self::Network => Some("Check your internet connection, then try again."),
            Self::RateLimited => Some(
                "The site is rate-limiting this IP. Wait a few minutes, or choose browser cookies \
                 to use a signed-in session.",
            ),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A link that the tab it was pasted into cannot use. `platform` and
    /// `accepted` come from [`crate::platform::Platform`], so the message names
    /// the tab rather than guessing at what the user meant.
    #[error("That doesn't look like a {platform} link: {url}")]
    InvalidUrl {
        platform: &'static str,
        accepted: &'static str,
        url: String,
    },

    #[error("Required tool `{name}` was not found")]
    MissingBinary { name: String },

    #[error("{message}")]
    Youtube {
        kind: YtErrorKind,
        message: String,
    },

    #[error("FFmpeg failed (exit {code}): {detail}")]
    Ffmpeg { code: i32, detail: String },

    #[error("Not enough free space on {volume}: need about {needed_mb} MB, {available_mb} MB available")]
    DiskSpace {
        volume: String,
        needed_mb: u64,
        available_mb: u64,
    },

    #[error("{0}")]
    TimeRange(String),

    #[error("Job cancelled")]
    Cancelled,

    #[error("Could not parse tool output: {0}")]
    Parse(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    /// Reject a link, quoting back the shapes the platform does accept.
    pub fn invalid_url(
        platform: &'static str,
        accepted: &'static str,
        url: impl Into<String>,
    ) -> Self {
        Self::InvalidUrl {
            platform,
            accepted,
            url: url.into(),
        }
    }

    /// Stable machine-readable discriminant for the frontend.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidUrl { .. } => "invalid_url",
            Self::MissingBinary { .. } => "missing_binary",
            Self::Youtube { kind, .. } => kind.as_str(),
            Self::Ffmpeg { .. } => "ffmpeg",
            Self::DiskSpace { .. } => "disk_space",
            Self::TimeRange(_) => "time_range",
            Self::Cancelled => "cancelled",
            Self::Parse(_) => "parse",
            Self::Io(_) => "io",
            Self::Other(_) => "other",
        }
    }

    pub fn hint(&self) -> Option<String> {
        match self {
            Self::Youtube { kind, .. } => kind.hint().map(str::to_string),
            Self::MissingBinary { name } => {
                // Each sidecar has its own vendoring script; naming the wrong
                // one would send the user to run something that does nothing
                // for the tool that's actually missing.
                let vendor_command = match name.as_str() {
                    "pandoc" => "npm run pandoc",
                    "pdfium" => "npm run pdfium",
                    "wkhtmltopdf" => "",
                    "whisper-cli" => "npm run whisper",
                    _ => "npm run binaries",
                };
                Some(if vendor_command.is_empty() {
                    format!(
                        "`{name}` is missing and is not vendored by this app — install it yourself \
                         and make sure it's on your PATH."
                    )
                } else {
                    format!(
                        "`{name}` is missing. Run `{vendor_command}` to vendor it into src-tauri/binaries, \
                         or install it so it's on your PATH."
                    )
                })
            }
            Self::InvalidUrl { accepted, .. } => Some(format!("Supported: {accepted}")),
            Self::DiskSpace { .. } => {
                Some("Free up space or choose an output folder on another drive.".to_string())
            }
            Self::Ffmpeg { .. } => Some(
                "The render pipeline failed. Try a different container, or switch the cut mode to \
                 \"Exact\" in Advanced."
                    .to_string(),
            ),
            _ => None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether retrying with freshly resolved URLs is likely to succeed.
    ///
    /// Google's media CDN intermittently answers a valid, unexpired URL with
    /// `403 Forbidden`; the same URL then works moments later. Observed in
    /// practice, so a single re-resolve-and-retry is worth the two seconds it
    /// costs rather than surfacing a spurious failure to the user.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Youtube { kind, .. } => {
                matches!(kind, YtErrorKind::Network | YtErrorKind::RateLimited)
            }
            Self::Ffmpeg { detail, .. } => {
                let d = detail.to_ascii_lowercase();
                d.contains("403")
                    || d.contains("forbidden")
                    || d.contains("server returned")
                    || d.contains("connection reset")
                    || d.contains("timed out")
                    || d.contains("end of file")
                    || d.contains("input/output error")
                    || d.contains("error opening input")
            }
            _ => false,
        }
    }
}

// Tauri requires command errors to be Serialize. A struct (not a bare string)
// keeps the failure actionable on the UI side.
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("AppError", 3)?;
        st.serialize_field("kind", self.kind())?;
        st.serialize_field("message", &self.to_string())?;
        st.serialize_field("hint", &self.hint())?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_failures_from_every_supported_site() {
        // Instagram and X almost always fail this way when signed out.
        assert_eq!(
            YtErrorKind::classify(
                "ERROR: [Instagram] ABC: Requested content is not available, rate-limit reached \
                 or login required"
            ),
            YtErrorKind::LoginRequired
        );
        assert_eq!(
            YtErrorKind::classify("ERROR: [twitter] 123: No video could be found in this tweet"),
            YtErrorKind::NotFound
        );
        assert_eq!(
            YtErrorKind::classify("ERROR: [TikTok] 123: This post has been deleted"),
            YtErrorKind::Removed
        );
        // A signed-out failure is actionable, so it must never fall through to
        // Unknown, which carries no hint.
        assert!(YtErrorKind::LoginRequired.hint().is_some());
    }

    #[test]
    fn classifies_common_youtube_failures() {
        assert_eq!(
            YtErrorKind::classify("ERROR: [youtube] abc: Private video. Sign in if you've been granted access"),
            YtErrorKind::Private
        );
        assert_eq!(
            YtErrorKind::classify("ERROR: Sign in to confirm your age. This video may be inappropriate for some users."),
            YtErrorKind::AgeRestricted
        );
        assert_eq!(
            YtErrorKind::classify("ERROR: Video unavailable. This video has been removed by the uploader"),
            YtErrorKind::Removed
        );
        assert_eq!(
            YtErrorKind::classify("ERROR: unable to download video data: HTTP Error 429: Too Many Requests"),
            YtErrorKind::RateLimited
        );
        assert_eq!(YtErrorKind::classify("some unrelated noise"), YtErrorKind::Unknown);
    }

    #[test]
    fn transient_failures_are_distinguished_from_permanent_ones() {
        // Observed against real googlevideo URLs: a valid URL 403s, then works.
        assert!(AppError::Ffmpeg {
            code: 1,
            detail: "Server returned 403 Forbidden (access denied)".into(),
        }
        .is_transient());

        assert!(AppError::Youtube {
            kind: YtErrorKind::Network,
            message: "no route".into(),
        }
        .is_transient());

        // Retrying these would only waste the user's time.
        assert!(!AppError::Youtube {
            kind: YtErrorKind::Private,
            message: "private".into(),
        }
        .is_transient());
        assert!(!AppError::Cancelled.is_transient());
        assert!(!AppError::Ffmpeg {
            code: 1,
            detail: "Unknown encoder 'h264_nvenc'".into(),
        }
        .is_transient());
    }

    #[test]
    fn every_classified_kind_serializes_to_a_stable_tag() {
        let e = AppError::Youtube {
            kind: YtErrorKind::GeoBlocked,
            message: "nope".into(),
        };
        assert_eq!(e.kind(), "geo_blocked");
        assert!(e.hint().is_some());
    }

    #[test]
    fn missing_binary_hints_name_the_right_vendoring_command() {
        let hint_for = |name: &str| AppError::MissingBinary { name: name.to_string() }.hint().unwrap();

        assert!(hint_for("pandoc").contains("npm run pandoc"));
        assert!(hint_for("pdfium").contains("npm run pdfium"));
        assert!(hint_for("whisper-cli").contains("npm run whisper"));
        assert!(hint_for("ffmpeg").contains("npm run binaries"));

        // wkhtmltopdf is deliberately never vendored (see convert::document::to_pdf),
        // so its hint must not send anyone to a command that vendors nothing.
        let wk = hint_for("wkhtmltopdf");
        assert!(!wk.contains("npm run"));
        assert!(wk.contains("PATH"));
    }
}

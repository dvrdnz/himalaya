use std::{collections::BTreeSet, fmt};

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local};
use clap::Parser;
use comfy_table::{Cell, Color, ContentArrangement, Row, Table};
use humansize::{BINARY, format_size};
use io_email::{address::Address, envelope::types::Envelope, flag::types::Flag};
use pimalaya_cli::printer::Printer;
use serde::Serialize;

use crate::account::context::Account;
use crate::shared::{client::EmailClient, mailbox::arg::MailboxArg};

const MINI_CONTENT_MAX_CHARS: usize = 48;
const FASTPLAIN_MAX_SCAN_BYTES: usize = 128 * 1024;
const CONTEXT_WINDOW_BYTES: usize = 120;
const LINK_POSITIVE_KEYWORDS: [&str; 9] = [
    "magic-link",
    "magiclink",
    "verify",
    "verification",
    "signin",
    "sign-in",
    "login",
    "auth",
    "confirm",
];
const LINK_NEGATIVE_KEYWORDS: [&str; 4] = ["unsubscribe", "support", "help", "preferences"];
const LINK_CONTEXT_POSITIVE_KEYWORDS: [&str; 9] = [
    "code",
    "otp",
    "passcode",
    "verification",
    "verify",
    "bestätigungscode",
    "bestaetigungscode",
    "authentication",
    "anmelden",
];
const LINK_CONTEXT_NEGATIVE_KEYWORDS: [&str; 3] = ["unsubscribe", "privacy policy", "impressum"];
const CODE_CONTEXT_KEYWORDS: [&str; 9] = [
    "code",
    "otp",
    "passcode",
    "verification",
    "verify",
    "bestätigungscode",
    "bestaetigungscode",
    "authentication",
    "sudo authentication",
];

/// List envelopes for the active account, regardless of the underlying
/// backend (IMAP, JMAP or Maildir).
///
/// Envelopes are ordered by date descending (most recent first). Use
/// `envelope search` to filter and/or sort with the shared search
/// query DSL.
#[derive(Debug, Parser)]
pub struct EnvelopeListCommand {
    #[command(flatten)]
    pub mailbox: MailboxArg,

    /// Page number, starting from 1. The most recent envelopes are on
    /// page 1.
    #[arg(long, short = 'p')]
    #[arg(value_name = "N", default_value = "1")]
    pub page: u32,

    /// Maximum number of envelopes per page.
    ///
    /// When omitted, the merged `envelope.list.page-size` config
    /// value is used; when neither is set, the hard fallback is 25.
    #[arg(long = "page-size", short = 's')]
    #[arg(value_name = "N")]
    pub page_size: Option<u32>,

    /// Maximum width of the rendered table, in terminal columns.
    ///
    /// Overrides comfy-table's auto-detection. Columns shrink with
    /// ellipsis if needed.
    #[arg(long = "max-width", short = 'w')]
    #[arg(value_name = "COLUMNS")]
    pub max_width: Option<u16>,

    /// Render recipients (`To:`) instead of senders (`From:`). Useful
    /// for sent folders.
    #[arg(long, short)]
    pub recipient: bool,

    /// Populate the ATT column. Free on JMAP; on IMAP this fetches
    /// `BODYSTRUCTURE` in addition to `ENVELOPE`; Maildir already
    /// parses the message body for subject/from/to so the toggle is
    /// essentially free there.
    #[arg(long = "has-attachment")]
    pub has_attachment: bool,

    /// Show compact plain output with only ID, FROM and CONTENT snippet.
    #[arg(long)]
    pub fastplain: bool,
}

impl EnvelopeListCommand {
    pub fn execute(
        self,
        printer: &mut impl Printer,
        account: &mut Account,
        client: &mut EmailClient,
    ) -> Result<()> {
        let page = Some(self.page).filter(|p| *p > 0);
        let page_size = self
            .page_size
            .or(Some(account.envelopes_list_page_size()))
            .filter(|p| *p > 0);
        let mailbox = self.mailbox.resolve(account)?;

        let envelopes = client.list_envelopes(&mailbox, page, page_size, self.has_attachment)?;

        if self.fastplain {
            let mini = MiniEnvelopes::from_envelopes(client, &mailbox, envelopes);
            return printer.out(mini);
        }

        let envelopes = Envelopes {
            preset: account.table_preset().to_string(),
            arrangement: account.table_arrangement(),
            max_width: self.max_width,
            datetime_fmt: account.datetime_fmt().to_string(),
            datetime_local_tz: account.datetime_local_tz(),
            recipient: self.recipient,
            with_attachment: self.has_attachment,
            chars: FlagChars {
                unseen: account.envelopes_list_table_unseen_char(),
                replied: account.envelopes_list_table_replied_char(),
                flagged: account.envelopes_list_table_flagged_char(),
                attachment: account.envelopes_list_table_attachment_char(),
            },
            colors: EnvelopeColors {
                id: account.envelopes_list_table_id_color(),
                flags: account.envelopes_list_table_flags_color(),
                att: account.envelopes_list_table_att_color(),
                subject: account.envelopes_list_table_subject_color(),
                from: account.envelopes_list_table_from_color(),
                to: account.envelopes_list_table_to_color(),
                date: account.envelopes_list_table_date_color(),
                size: account.envelopes_list_table_size_color(),
            },
            envelopes,
        };

        printer.out(envelopes)
    }
}

/// Glyphs the FLAGS / ATT columns substitute in, sourced from the
/// merged account config (v1.2.0 defaults: `*`, `R`, `!`, `@`).
#[derive(Clone, Copy, Debug)]
pub(super) struct FlagChars {
    pub unseen: char,
    pub replied: char,
    pub flagged: char,
    pub attachment: char,
}

/// Per-column foreground colors for the envelopes table. `Color::Reset`
/// means "use the terminal default" (i.e. no override).
#[derive(Clone, Copy, Debug)]
pub(super) struct EnvelopeColors {
    pub id: Color,
    pub flags: Color,
    pub att: Color,
    pub subject: Color,
    pub from: Color,
    pub to: Color,
    pub date: Color,
    pub size: Color,
}

/// Compact envelope rows for fast, script-friendly list output.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
struct MiniEnvelopes(Vec<MiniEnvelope>);

#[derive(Clone, Debug, Serialize)]
struct MiniEnvelope {
    id: String,
    from: String,
    content: String,
}

impl MiniEnvelopes {
    fn from_envelopes(client: &mut EmailClient, mailbox: &str, envelopes: Vec<Envelope>) -> Self {
        let envelopes = envelopes
            .into_iter()
            .map(|env| {
                let content = client
                    .get_message(mailbox, &env.id)
                    .map(|raw| Self::preview_from_raw(&raw))
                    .unwrap_or_default();

                MiniEnvelope {
                    id: env.id,
                    from: format_addresses(&env.from),
                    content,
                }
            })
            .collect();

        Self(envelopes)
    }

    fn sanitize_content(content: &str) -> String {
        let mut content = content.to_owned();

        while let Some(start) = content.find("<#part") {
            let Some(end_rel) = content[start..].find('>') else {
                break;
            };
            let end = start + end_rel + 1;
            content.replace_range(start..end, " ");
        }

        content = content.replace("=\r\n", "");
        content = content.replace("=\n", "");
        content = content.replace("=3D", "=");
        content = content.replace("=3d", "=");

        content
    }

    fn normalize_spaces(content: &str) -> String {
        content.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn compact_content(content: &str) -> String {
        let content = Self::normalize_spaces(content);
        let len = content.chars().count();

        if len <= MINI_CONTENT_MAX_CHARS {
            content
        } else {
            let cut = MINI_CONTENT_MAX_CHARS.saturating_sub(3);
            format!("{}...", content.chars().take(cut).collect::<String>())
        }
    }

    fn contains_any(haystack: &str, needles: &[&str]) -> bool {
        needles.iter().any(|needle| haystack.contains(needle))
    }

    fn char_boundary_at_or_before(content: &str, mut idx: usize) -> usize {
        while idx > 0 && !content.is_char_boundary(idx) {
            idx -= 1;
        }

        idx
    }

    fn has_context_keyword_near(
        lower_content: &str,
        needle_lower: &str,
        keywords: &[&str],
    ) -> bool {
        if needle_lower.is_empty() {
            return false;
        }

        let mut start = 0;
        while let Some(rel) = lower_content[start..].find(needle_lower) {
            let idx = start + rel;
            let ctx_start = Self::char_boundary_at_or_before(
                lower_content,
                idx.saturating_sub(CONTEXT_WINDOW_BYTES),
            );
            let ctx_end = Self::char_boundary_at_or_before(
                lower_content,
                (idx + needle_lower.len() + CONTEXT_WINDOW_BYTES).min(lower_content.len()),
            );
            if Self::contains_any(&lower_content[ctx_start..ctx_end], keywords) {
                return true;
            }
            start = idx.saturating_add(needle_lower.len());
            if start >= lower_content.len() {
                break;
            }
        }

        false
    }

    fn decode_qp_urlish(input: &str) -> String {
        fn hex_value(b: u8) -> Option<u8> {
            match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(10 + (b - b'a')),
                b'A'..=b'F' => Some(10 + (b - b'A')),
                _ => None,
            }
        }

        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] == b'=' {
                if i + 2 < bytes.len() {
                    if let (Some(h1), Some(h2)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
                    {
                        let decoded = (h1 << 4) | h2;
                        if decoded.is_ascii_graphic() || decoded == b' ' {
                            out.push(decoded);
                            i += 3;
                            continue;
                        }
                    }
                }

                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 2;
                    continue;
                }

                if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                    i += 3;
                    continue;
                }
            }

            out.push(bytes[i]);
            i += 1;
        }

        String::from_utf8_lossy(&out).into_owned()
    }

    fn is_repeated_digits(code: &str) -> bool {
        let mut chars = code.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        first.is_ascii_digit() && chars.all(|ch| ch == first)
    }

    fn raw_body(raw: &str) -> &str {
        if let Some(idx) = raw.find("\r\n\r\n") {
            return &raw[idx + 4..];
        }
        if let Some(idx) = raw.find("\n\n") {
            return &raw[idx + 2..];
        }
        raw
    }

    fn extract_links(content: &str) -> Vec<String> {
        let mut links = Vec::<String>::new();
        let mut push_link = |link: &str| {
            let link = Self::decode_qp_urlish(link);
            let link = link.trim_matches(|c: char| "\"'()[]<>,.;:".contains(c));
            if link.starts_with("http://")
                || link.starts_with("https://")
                || link.starts_with("www.")
            {
                let link = link.to_owned();
                if !links.contains(&link) {
                    links.push(link);
                }
            }
        };

        let mut i = 0;
        while i < content.len() {
            let Some(rel) = content[i..].find("href=") else {
                break;
            };

            let start = i + rel + 5;
            let rest = &content[start..];
            if rest.is_empty() {
                break;
            }

            let (link_start, quoted) = match rest.chars().next() {
                Some('"') | Some('\'') => (start + 1, true),
                _ => (start, false),
            };

            let end = if quoted {
                content[link_start..]
                    .find(|c| c == '"' || c == '\'')
                    .map(|e| link_start + e)
            } else {
                content[link_start..]
                    .find(|c: char| c.is_whitespace() || "<>'\"".contains(c))
                    .map(|e| link_start + e)
            }
            .unwrap_or(content.len());

            push_link(&content[link_start..end]);
            i = end.saturating_add(1).min(content.len());
        }

        let mut j = 0;
        while j < content.len() {
            let Some(rel) = ["https://", "http://", "www."]
                .iter()
                .filter_map(|needle| content[j..].find(needle))
                .min()
            else {
                break;
            };

            let start = j + rel;
            let end = content[start..]
                .find(|c: char| c.is_whitespace() || "<>'\"".contains(c))
                .map(|e| start + e)
                .unwrap_or(content.len());
            push_link(&content[start..end]);
            j = end.saturating_add(1).min(content.len());
        }

        links
    }

    fn score_link(lower_content: &str, link: &str) -> i32 {
        let link_lower = link.to_ascii_lowercase();
        let mut score = 0;
        let has_positive_link_keyword = Self::contains_any(&link_lower, &LINK_POSITIVE_KEYWORDS);

        if has_positive_link_keyword {
            score += 260;
        }

        if Self::contains_any(&link_lower, &LINK_NEGATIVE_KEYWORDS) {
            score -= 220;
        }

        let has_positive_ctx = Self::has_context_keyword_near(
            lower_content,
            &link_lower,
            &LINK_CONTEXT_POSITIVE_KEYWORDS,
        );
        if has_positive_ctx {
            score += 180;
        }

        let has_negative_ctx = Self::has_context_keyword_near(
            lower_content,
            &link_lower,
            &LINK_CONTEXT_NEGATIVE_KEYWORDS,
        );
        if has_negative_ctx {
            score -= 160;
        }

        if !has_positive_link_keyword && !has_positive_ctx {
            score -= 120;
        }

        score
    }

    fn select_verification_link(content: &str) -> Option<String> {
        let lower_content = content.to_ascii_lowercase();

        Self::extract_links(content)
            .into_iter()
            .map(|link| (Self::score_link(&lower_content, &link), link))
            .max_by_key(|(score, _)| *score)
            .filter(|(score, _)| *score >= 150)
            .map(|(_, link)| link)
    }

    fn extract_code_candidates(content: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        let mut token = String::new();

        let push_token = |token: &str, candidates: &mut Vec<String>| {
            if token.is_empty() {
                return;
            }

            let token = token.trim_matches('-');
            let len = token.chars().count();
            let has_digit = token.chars().any(|ch| ch.is_ascii_digit());
            let allowed = token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-');

            if has_digit && allowed && (4..=12).contains(&len) {
                candidates.push(token.to_owned());
            }
        };

        for ch in content.chars() {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                token.push(ch);
            } else {
                push_token(&token, &mut candidates);
                token.clear();
            }
        }
        push_token(&token, &mut candidates);

        candidates
    }

    fn score_code(lower_content: &str, code: &str) -> i32 {
        let mut score = 0;
        let len = code.chars().count();
        let digits = code.chars().filter(|ch| ch.is_ascii_digit()).count();
        let letters = code.chars().filter(|ch| ch.is_ascii_alphabetic()).count();
        let code_lower = code.to_ascii_lowercase();
        let has_code_ctx =
            Self::has_context_keyword_near(lower_content, &code_lower, &CODE_CONTEXT_KEYWORDS);

        if digits == len {
            if (6..=8).contains(&len) {
                score += 180;
            } else if (4..=10).contains(&len) {
                score += 90;
            }

            if Self::is_repeated_digits(code) {
                score -= 240;
            }
        } else if digits >= 4 && letters > 0 {
            score += 80;
        }

        if len == 4 && code.starts_with("20") {
            score -= 150;
        }

        if has_code_ctx {
            score += 180;
        } else {
            score -= 220;
        }

        score
    }

    fn select_verification_code(content: &str) -> Option<String> {
        let lower_content = content.to_ascii_lowercase();

        Self::extract_code_candidates(content)
            .into_iter()
            .map(|code| (Self::score_code(&lower_content, &code), code))
            .max_by_key(|(score, _)| *score)
            .filter(|(score, _)| *score >= 200)
            .map(|(_, code)| code)
    }

    fn select_content_snippet(content: &str) -> String {
        let sanitized = Self::sanitize_content(content);

        if let Some(link) = Self::select_verification_link(&sanitized) {
            return Self::compact_content(&link);
        }

        if let Some(code) = Self::select_verification_code(&sanitized) {
            return Self::compact_content(&code);
        }

        Self::compact_content(&sanitized)
    }

    fn preview_from_raw(raw: &[u8]) -> String {
        let len = raw.len().min(FASTPLAIN_MAX_SCAN_BYTES);
        let raw = String::from_utf8_lossy(&raw[..len]).into_owned();
        let body = Self::raw_body(&raw);

        Self::select_content_snippet(body)
    }
}

impl fmt::Display for MiniEnvelopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "ID\tFROM\tCONTENT")?;

        for env in self.0.iter() {
            writeln!(f, "{}\t{}\t{}", env.id, env.from, env.content)?;
        }

        Ok(())
    }
}

/// Table of envelope rows rendered to the terminal or as JSON.
#[derive(Clone, Debug, Serialize)]
pub struct Envelopes {
    #[serde(skip)]
    pub preset: String,
    #[serde(skip)]
    pub arrangement: ContentArrangement,
    #[serde(skip)]
    pub max_width: Option<u16>,
    #[serde(skip)]
    pub datetime_fmt: String,
    #[serde(skip)]
    pub datetime_local_tz: bool,
    #[serde(skip)]
    pub recipient: bool,
    #[serde(skip)]
    pub with_attachment: bool,
    #[serde(skip)]
    pub(super) chars: FlagChars,
    #[serde(skip)]
    pub(super) colors: EnvelopeColors,
    pub envelopes: Vec<Envelope>,
}

impl fmt::Display for Envelopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut table = Table::new();

        let mut header = vec![Cell::new("ID"), Cell::new("FLAGS")];
        if self.with_attachment {
            header.push(Cell::new("ATT"));
        }
        header.push(Cell::new("SUBJECT"));
        header.push(Cell::new(if self.recipient { "TO" } else { "FROM" }));
        header.push(Cell::new("DATE"));
        header.push(Cell::new("SIZE"));

        table
            .load_preset(&self.preset)
            .set_content_arrangement(self.arrangement.clone())
            .set_header(Row::from(header))
            .add_rows(self.envelopes.iter().map(|env| {
                let mut row = Row::new();
                row.max_height(1);
                row.add_cell(Cell::new(&env.id).fg(self.colors.id));
                row.add_cell(
                    Cell::new(format_flags(&env.flags, &self.chars)).fg(self.colors.flags),
                );
                if self.with_attachment {
                    row.add_cell(
                        Cell::new(format_attachment(env.has_attachment, self.chars.attachment))
                            .fg(self.colors.att),
                    );
                }
                row.add_cell(Cell::new(&env.subject).fg(self.colors.subject));

                let addresses = if self.recipient { &env.to } else { &env.from };
                let from_or_to_color = if self.recipient {
                    self.colors.to
                } else {
                    self.colors.from
                };
                row.add_cell(Cell::new(format_addresses(addresses)).fg(from_or_to_color));

                row.add_cell(
                    Cell::new(format_date(
                        env.date,
                        &self.datetime_fmt,
                        self.datetime_local_tz,
                    ))
                    .fg(self.colors.date),
                );
                row.add_cell(Cell::new(format_size(env.size, BINARY)).fg(self.colors.size));
                row
            }));

        if let Some(width) = self.max_width {
            table.set_width(width);
        }

        writeln!(f)?;
        writeln!(f, "{table}")
    }
}

/// 3-character flag widget: unseen, replied, flagged. Each slot is a
/// space when the flag is absent, otherwise the configured glyph
/// (v1.2.0 defaults: `*`, `R`, `!`).
pub(super) fn format_flags(flags: &BTreeSet<Flag>, chars: &FlagChars) -> String {
    let mut out = String::with_capacity(3);
    out.push(if flags.iter().any(Flag::is_seen) {
        ' '
    } else {
        chars.unseen
    });
    out.push(if flags.iter().any(Flag::is_answered) {
        chars.replied
    } else {
        ' '
    });
    out.push(if flags.iter().any(Flag::is_flagged) {
        chars.flagged
    } else {
        ' '
    });
    out
}

pub(super) fn format_attachment(has: Option<bool>, glyph: char) -> String {
    match has {
        Some(true) => glyph.to_string(),
        Some(false) => String::new(),
        None => "?".to_string(),
    }
}

pub(super) fn format_addresses(addrs: &[Address]) -> String {
    addrs
        .iter()
        .map(|a| match &a.name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => a.email.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn format_date(
    date: Option<DateTime<FixedOffset>>,
    fmt: &str,
    local_tz: bool,
) -> String {
    let Some(date) = date else {
        return String::new();
    };
    if local_tz {
        date.with_timezone(&Local).format(fmt).to_string()
    } else {
        date.format(fmt).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fastplain_prefers_verification_link() {
        let content = r#"Ignore https://example.test/help
            <a href="https://example.test/auth/verify?token=abc123">Verify</a>"#;

        assert_eq!(
            MiniEnvelopes::select_content_snippet(content),
            "https://example.test/auth/verify?token=abc123"
        );
    }

    #[test]
    fn fastplain_ignores_unsubscribe_link_for_code() {
        let content = "Verification code: 814209. Unsubscribe at https://example.test/unsubscribe";

        assert_eq!(MiniEnvelopes::select_content_snippet(content), "814209");
    }

    #[test]
    fn fastplain_decodes_quoted_printable_url() {
        let content = r#"<a href="https://example.test/login?token=3Dabc=3D123">Sign in</a>"#;

        assert_eq!(
            MiniEnvelopes::select_content_snippet(content),
            "https://example.test/login?token=abc=123"
        );
    }

    #[test]
    fn fastplain_compacts_long_content() {
        let content =
            "This is a regular message body with enough words to exceed the mini output limit";

        assert_eq!(
            MiniEnvelopes::select_content_snippet(content),
            "This is a regular message body with enough wo..."
        );
    }

    #[test]
    fn fastplain_handles_non_ascii_context_windows() {
        let content = format!("{} code 739521", "ä".repeat(130));

        assert_eq!(MiniEnvelopes::select_content_snippet(&content), "739521");
    }
}

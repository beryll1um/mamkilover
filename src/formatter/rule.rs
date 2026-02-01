// Copyright (c) 2025 Mamkilover's
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

use teloxide::types::{MessageEntity, MessageEntityKind};

use regex::Regex;
use std::collections::BTreeSet;
use url::{Url, form_urlencoded};

/// Describes whether a rule modified at least one URL.
pub enum Effect {
    Mutated,
    Skipped,
}
use Effect::*;

/// A formatting rule that can inspect and possibly mutate URLs
/// contained in message text and entities.
pub trait Rule {
    fn apply(&self, text: &mut Vec<u16>, entities: &mut Vec<MessageEntity>) -> Effect;
}

/// Converts a possibly relative or incomplete URL into an absolute URL,
/// making reasonable assumptions about the protocol if needed.
fn parse_with_default_scheme(s: &str) -> Result<Url, url::ParseError> {
    if s.contains("://") {
        Url::parse(s)
    } else {
        Url::parse(&format!("https://{s}"))
    }
}

/// Iterates over all URL-related entities and applies a mutation function.
///
/// This function is responsible for:
/// - keeping entity offsets in sync with text mutations,
/// - rewriting inline URL text when necessary,
/// - tracking whether at least one mutation occurred.
fn for_each_url_mut<F>(
    text: &mut Vec<u16>, entities: &mut Vec<MessageEntity>, mut try_mutate_url: F
) -> Effect
where
    F: FnMut(&mut Url) -> Effect
{
    let mut effect = Skipped;
    // Accumulates length changes caused by previous rewrites so subsequent
    // entity offsets remain aligned with the modified text buffer.
    let mut offset_delta: isize = 0;
    for entity in entities {
        // Adjust the entity offset to account for earlier mutations.
        entity.offset = (entity.offset as isize + offset_delta) as usize;
        match entity.kind {
            MessageEntityKind::Url => {
                // Extract the UTF-16 range currently representing this URL.
                let range = entity.offset .. entity.offset + entity.length;
                let utf8 = &String::from_utf16_lossy(&text[range.clone()]);
                // Telegram should never send URLs that are not parsable, but there is one
                // special case with URLs that are missing a protocol (scheme).
                let Ok(mut url) = parse_with_default_scheme(utf8) else {
                    log::error!("Unable to parse the URL from a message: {}", utf8);
                    continue
                };
                if matches!(try_mutate_url(&mut url), Mutated) {
                    let utf16 = url.as_str().encode_utf16();
                    // Replace the original URL text with the rewritten one.
                    text.splice(range.clone(), utf16.clone());
                    // Compute how much the URL length changed in UTF-16 code units.
                    let delta = utf16.count() as isize - range.len() as isize;
                    entity.length = (entity.length as isize + delta) as usize;
                    offset_delta += delta;
                    // Mark that at least one URL mutation occurred.
                    effect = Mutated
                }
            },
            MessageEntityKind::TextLink { ref mut url } => {
                // TextLink entities do not affect the message text length,
                // so only the embedded URL needs to be updated.
                if matches!(try_mutate_url(url), Mutated) {
                    effect = Mutated
                }
            },
            _ => {}
        }
    }
    effect
}

/// Replaces the host of matching URLs with a fixed value.
pub struct UrlHostReplaceRule {
    pub host_regex: Regex,
    pub replace: String,
}

impl Rule for UrlHostReplaceRule {
    fn apply(&self, text: &mut Vec<u16>, entities: &mut Vec<MessageEntity>) -> Effect {
        for_each_url_mut(text, entities, |url: &mut Url| {
            // Telegram URLs are expected to always have a host.
            // Missing hosts indicate malformed input or unexpected entity data.
            let Some(host) = url.host_str() else {
                log::warn!("Skipping URL with missing host: {}", url);
                return Skipped
            };
            // Ignore URLs whose host does not match the configured filter.
            if !self.host_regex.is_match(host) {
                return Skipped
            }
            // Avoid producing invalid URLs by rejecting invalid replacement hosts.
            if let Err(_) = url.set_host(Some(&self.replace)) {
                log::warn!("Attempting to set invalid host: {}", self.replace);
                return Skipped
            }
            Mutated
        })
    }
}

/// Removes selected query parameters from matching URLs.
pub struct UrlQueryExcludeRule {
    pub host_regex: Regex,
    pub exclude: BTreeSet<String>,
}

impl UrlQueryExcludeRule {
    /// Returns a rewritten query string if exclusions occurred,
    /// or `None` if the URL should remain unchanged.
    fn filter_or_none(&self, url: &Url) -> Option<String> {
        // Telegram URLs are expected to always have a host.
        let Some(host) = url.host_str() else {
            log::warn!("Skipping URL with missing host: {}", url);
            return None
        };
        // Skip URLs whose host does not match the configured filter.
        if !self.host_regex.is_match(host) {
            return None
        }
        // Tracks whether any excluded parameters were encountered.
        let mut has_exclusions = false;
        // Rebuild the query string while preserving parameter order
        // and dropping only excluded keys.
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (k, v) in url.query_pairs() {
            if self.exclude.contains(k.as_ref()) {
                has_exclusions = true;
            } else {
                serializer.append_pair(&k, &v);
            }
        }
        // If nothing was removed, no rewrite is necessary.
        if !has_exclusions {
            return None
        }
        // An empty string signals that the query should be removed entirely.
        Some(serializer.finish())
    }
}

impl Rule for UrlQueryExcludeRule {
    fn apply(&self, text: &mut Vec<u16>, entities: &mut Vec<MessageEntity>) -> Effect {
        for_each_url_mut(text, entities, |url: &mut Url| {
            let Some(query) = self.filter_or_none(url) else {
                return Skipped
            };
            url.set_query(if query.is_empty() { None } else { Some(query.as_str()) });
            Mutated
        })
    }
}


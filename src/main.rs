// Copyright (c) 2025 Mamkilover's
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

mod formatter;

use regex::Regex;
use url::Url;

use std::{
    env,
    fmt::Write,
    borrow::Cow,
    collections::BTreeSet,
};

use teloxide::{
    prelude::*,
    types::{
        Recipient,
        ParseMode,
        MessageEntity,
        MessageEntityKind,
        ChatKind,
        PublicChatKind,
        ReplyParameters,
        LinkPreviewOptions,
        InlineKeyboardMarkup,
        InlineKeyboardButton,
    },
    RequestError,
    utils::render::Renderer,
};

use diesel::{
    prelude::*,
    r2d2::{
        Pool,
        ConnectionManager,
    },
    sqlite::{
        Sqlite,
        SqliteConnection,
    }
};

/*
Frankly, I'm a lazy ass and don't want to mess around with a database and
automated migrations for such a small project.
Just create this table yourself:
```sql
CREATE TABLE IF NOT EXISTS formatted_messages (
    message_id INT,
    user_tgid UNSIGNED BIG INT,
    chat_tgid BIG INT,
    PRIMARY KEY (chat_tgid, message_id)
);
```
*/
diesel::table! {
    formatted_messages (chat_tgid, message_id) {
        message_id -> Integer,
        // SQLite maps `UNSIGNED BIG INT` to INTEGER internally.
        // Diesel represents this as `BigInt` (i64).
        user_tgid -> BigInt,
        chat_tgid -> BigInt,
    }
}

#[derive(diesel::Insertable)]
#[diesel(table_name = formatted_messages)]
struct NewFormattedMessage {
    message_id: i32,
    user_tgid: i64,
    chat_tgid: i64,
}

/// Returns a human-readable mention for the message sender.
/// Prefers `@username` when available, otherwise falls back to the first name.
/// Returns `None` if the message has no sender.
fn sender_mention<'a>(msg: &'a Message) -> Option<Cow<'a, String>> {
    if let Some(user) = &msg.from {
        if let Some(username) = &user.username {
            Some(Cow::Owned(format!("@{}", username)))
        } else {
            Some(Cow::Borrowed(&user.first_name))
        }
    } else {
        None
    }
}

/// Attempts to construct a public t.me link to a message.
/// Returns `None` for private chats and basic groups, which do not support links.
fn message_link(msg: &Message) -> Option<String> {
    // Private chats and basic groups cannot be linked publicly.
    match &msg.chat.kind {
        ChatKind::Private(_) => {
            return None;
        }
        ChatKind::Public(public) => {
            if matches!(public.kind, PublicChatKind::Group) {
                return None;
            }
        }
    }
    // Use username when available, otherwise derive the internal supergroup ID.
    let to_link = if let Some(username) = msg.chat.username() {
        username.into()
    } else {
        // Supergroup IDs start with "-100", which must be stripped for t.me links.
        format!("c/{}", &msg.chat.id.0.to_string()[4..])
    };
    Some(format!("https://t.me/{}/{}", to_link, msg.id.0))
}

/// Applies URL normalization and replacement rules to a message.
/// If formatting is performed, the original message is replaced and recorded.
/// Returns the newly sent message when formatting occurs, otherwise `None`.
async fn maybe_format_message<C>(
    bot: &Bot, msg: &Message, sqlite_conn: &mut C
) -> Result<Option<Message>, RequestError>
where
    C: Connection<Backend = Sqlite>,
{
    // Ensure the message has both entities and text; otherwise there is nothing to process.
    let mut entities = match msg.entities() {
        Some(entities) => entities.to_vec(),
        None => return Ok(None),
    };
    let mut text: Vec<u16> = match msg.text() {
        Some(text) => text.trim_end().encode_utf16().collect(),
        None => return Ok(None),
    };
    if {
        // Regex compilation happens at runtime but its acceptable (at least for now).
        let twitter_regex = Regex::new(r"^(?:www\.)?(?:twitter|x)\.com$")
            .expect("valid X (Twitter) host regex");
        let rules: [&dyn formatter::Rule; 5] = [
            // X (Twitter): remove tracking query parameters.
            &formatter::UrlQueryExcludeRule {
                host_regex: twitter_regex.clone(),
                exclude: BTreeSet::from([
                    "s".into(),
                    "t".into(),
                ]),
            },
            // X (Twitter): replace host with an IV friendly frontend.
            &formatter::UrlHostReplaceRule {
                host_regex: twitter_regex,
                replace: "i.fixupx.com".into(),
            },
            // TikTok: replace host with an IV friendly frontend.
            &formatter::UrlHostReplaceRule {
                host_regex: Regex::new(r"^(?:vm\.|www\.)?(?:tiktok)\.com$")
                    .expect("valid TikTok host regex"),
                replace: "fixtiktok.com".into(),
            },
            // YouTube: strip unnecessary share parameters.
            &formatter::UrlQueryExcludeRule {
                host_regex: Regex::new(r"^(?:www\.)?(youtube\.com|youtu\.be)$")
                    .expect("valid YouTube host regex"),
                exclude: BTreeSet::from([
                    "si".into(),
                    "pp".into(),
                ]),
            },
            // Instagram: remove share hash parameters.
            &formatter::UrlQueryExcludeRule {
                host_regex: Regex::new(r"^(?:www\.)?instagram\.com$")
                    .expect("valid Instagram host regex"),
                exclude: BTreeSet::from([
                    "igsh".into(),
                ]),
            },
        ];
        // If every rule reports `Skipped`, the message does not require formatting.
        rules.iter().fold(true, |acc, rule| {
            acc & matches!(rule.apply(&mut text, &mut entities), formatter::Effect::Skipped)
        })
    } {
        return Ok(None);
    }
    // Wrap the entire message in a blockquote for visual distinction.
    entities.push(
        MessageEntity::new(MessageEntityKind::Blockquote, 0, text.len())
    );
    // Convert UTF-16 buffer back to UTF-8 as required by Telegram.
    let mut text = String::from_utf16_lossy(&text);
    // Append the original author's mention as a footer.
    let Some(mention) = sender_mention(msg) else {
        log::error!("Failed to obtain sender mention: MessageId={}|ChatId={}",
            msg.id, msg.chat.id);
        return Ok(None);
    };
    if let Err(_) = write!(&mut text, "\n— {}", mention) {
        log::error!("Failed to write sender mention to replacement message footer.");
        return Ok(None);
    }
    // Send the formatted replacement message.
    let mut send_message = bot
        .send_message(msg.chat.id, text)
        .entities(entities)
        .link_preview_options(LinkPreviewOptions {
            is_disabled: false,
            url: None,
            prefer_small_media: false,
            prefer_large_media: true,
            show_above_text: true,
        })
        .disable_notification(true);
    if let Some(reply_msg) = msg.reply_to_message() {
        send_message = send_message.reply_parameters(ReplyParameters {
            message_id: reply_msg.id,
            ..ReplyParameters::default()
        });
    }
    let formatted_msg = send_message.await?;
    // Store the formatted message to support reply-based notifications later.
    if let Err(err) = diesel::insert_into(formatted_messages::table)
        .values(NewFormattedMessage {
            message_id: formatted_msg.id.0,
            user_tgid: msg.from.as_ref().map_or_else(|| {
                    log::warn!("Invalid empty UserId assigned to MessageId={}|ChatId={}",
                        formatted_msg.id.0, formatted_msg.chat.id.0);
                    0
                }, |user| user.id.0 as i64),
            chat_tgid: formatted_msg.chat.id.0,
        })
        .execute(sqlite_conn)
    {
        log::error!("Failed to insert formatted message: {}", err);
    }
    // Remove the original unformatted message.
    bot.delete_message(msg.chat.id, msg.id).await?;
    Ok(Some(formatted_msg))
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let sqlite_pool = Pool::builder()
        .max_size(8)
        .build(
            ConnectionManager::<SqliteConnection>::new(
                env::var("MAMKILOVER_SQLITE_PATH").expect("path to SQLite database file"),
            ),
        )
        .unwrap();
    // Registers the main message handler and starts polling.
    teloxide::repl(Bot::from_env(), move |bot: Bot, msg: Message| {
        let sqlite_pool = sqlite_pool.clone();
        async move {
            let mut sqlite_conn = match sqlite_pool.get() {
                Err(err) => {
                    log::error!("Failed to get SQLite connection: {}", err);
                    return Ok(());
                },
                Ok(conn) => conn,
            };
            // Attempt to format the message; fallback to original if not formatted.
            let formatted_msg = maybe_format_message(&bot, &msg, &mut sqlite_conn).await?;
            let effective_msg = match formatted_msg.as_ref() {
                None => &msg, Some(formatted) => formatted,
            };

            // Only replies can trigger notifications.
            let Some(replied_msg) = msg.reply_to_message() else {
                return Ok(());
            };
            // Look up whether the replied message was previously formatted.
            let maybe_user_id = match formatted_messages::table
                .filter(formatted_messages::message_id.eq(replied_msg.id.0))
                .filter(formatted_messages::chat_tgid.eq(replied_msg.chat.id.0))
                .select(formatted_messages::user_tgid)
                .first::<i64>(&mut sqlite_conn)
                .optional()
            {
                Err(err) => {
                    log::error!(
                        "Failed to select formatted message: MessageId={}|ChatId={} err={}",
                        replied_msg.id.0,
                        replied_msg.chat.id.0,
                        err
                    );
                    None
                },
                Ok(ok) => ok,
            };
            // If not found, the replied message was not generated by formatting bot.
            let Some(user_id) = maybe_user_id else {
                return Ok(());
            };
            // Avoid notifying the author about their own reply.
            if user_id == msg.from.as_ref().map_or(0, |user| user.id.0 as i64) {
                return Ok(());
            }
            let Some(mention) = sender_mention(&msg) else {
                log::error!("Failed to obtain sender mention: MessageId={}|ChatId={}",
                    msg.id, msg.chat.id);
                return Ok(());
            };

            // Messages without public links cannot be notified.
            let Some(Ok(msg_link)) = message_link(&effective_msg)
                    .map(|link| Url::parse(&link)) else {
                return Ok(());
            };
            // Render the effective message using HTML to simplify things.
            let effective_html = Renderer::new(
                effective_msg.text().unwrap_or_default(),
                effective_msg.entities().unwrap_or_default(),
            )
            .as_html();
            // Notify the original author about the reply.
            bot.send_message(
                UserId(user_id as u64),
                format!(
                    "<blockquote>{}</blockquote>\n— {}",
                    effective_html,
                    mention
                )
            )
            .parse_mode(ParseMode::Html)
            .link_preview_options(LinkPreviewOptions {
                is_disabled: true,
                url: None,
                prefer_small_media: false,
                prefer_large_media: false,
                show_above_text: false,
            })
            .reply_parameters(ReplyParameters{
                message_id: replied_msg.id,
                chat_id: Some(Recipient::Id(replied_msg.chat.id)),
                ..ReplyParameters::default()
            })
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::url(
                    "View in chat",
                    msg_link,
                ),
            ]]))
            .await?;
            Ok(())
        }
    })
    .await;
}


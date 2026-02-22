// Copyright (c) 2025 Mamkilover's
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

mod formatter;

use std::{
	borrow::Cow,
	env,
	fmt::Write,
};

use diesel::{
	prelude::*,
	r2d2::{
		ConnectionManager,
		Pool,
	},
	sqlite::SqliteConnection,
};
use regex::Regex;
use teloxide::{
	RequestError,
	prelude::*,
	types::{
		ChatKind,
		InlineKeyboardButton,
		InlineKeyboardMarkup,
		LinkPreviewOptions,
		MessageEntity,
		MessageEntityKind,
		MessageId,
		ParseMode,
		PublicChatKind,
		Recipient,
		ReplyParameters,
	},
	utils::render::Renderer,
};
use url::Url;

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
CREATE TABLE IF NOT EXISTS formatted_replies (
	message_id INT,
	user_tgid UNSIGNED BIG INT,
	source_message_id INT,
	source_chat_tgid BIG INT,
	PRIMARY KEY (source_message_id, source_chat_tgid)
);
```
SQLite maps `UNSIGNED BIG INT` to `INTEGER` internally.
Diesel represents this as `BigInt` (i64).
*/
diesel::table! {
	formatted_messages (chat_tgid, message_id) {
		message_id -> Integer,
		user_tgid -> BigInt,
		chat_tgid -> BigInt,
	}
}
diesel::table! {
	formatted_replies (source_message_id, source_chat_tgid) {
		message_id -> Integer,
		user_tgid -> BigInt,
		source_message_id -> Integer,
		source_chat_tgid -> BigInt,
	}
}

#[derive(diesel::Insertable)]
#[diesel(table_name = formatted_messages)]
struct NewFormattedMessage {
	message_id: i32,
	user_tgid: i64,
	chat_tgid: i64,
}

#[derive(diesel::Insertable)]
#[diesel(table_name = formatted_replies)]
struct NewFormattedReply {
	message_id: i32,
	user_tgid: i64,
	source_message_id: i32,
	source_chat_tgid: i64,
}

/// Attempts to construct a public t.me link to a message.
/// Returns `None` for private chats and basic groups,
/// which do not support links.
fn message_link(msg: &Message) -> Option<Url> {
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
	// Use username when available, otherwise
	// derive the internal supergroup ID.
	let to_link = if let Some(username) = msg.chat.username() {
		username.into()
	} else {
		// Supergroup IDs start with "-100", which must
		// be stripped for t.me links.
		format!("c/{}", &msg.chat.id.0.to_string()[4..])
	};
	Url::parse(&format!("https://t.me/{}/{}", to_link, msg.id)).ok()
}

/// Applies URL normalization and replacement rules to a message.
/// If formatting is performed, the original message is replaced and recorded.
/// Returns the newly sent message when formatting occurs, otherwise `None`.
async fn maybe_format_message(
	bot: &Bot,
	msg: &Message,
	quote: bool,
) -> Result<Option<Message>, RequestError> {
	// Ensure the message has both entities and text;
	// otherwise there is nothing to process.
	let mut entities = match msg.entities() {
		Some(entities) => entities.to_vec(),
		None => return Ok(None),
	};
	let mut utf16_text = match msg.text() {
		Some(text) => text.trim_end().encode_utf16().collect(),
		None => return Ok(None),
	};
	if {
		// Regex compilation happens at runtime but its acceptable
		// (at least for now).
		let twitter_regex = Regex::new(r"^(?:www\.)?(?:twitter|x)\.com$")
			.expect("valid X (Twitter) host regex");
		let rules: [&dyn formatter::Rule; 5] = [
			// X (Twitter): remove tracking query parameters.
			&formatter::UrlQueryExcludeRule {
				host_regex: twitter_regex.clone(),
				exclude: ["s".into(), "t".into()].into(),
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
				exclude: ["si".into(), "pp".into()].into(),
			},
			// Instagram: remove share hash parameters.
			&formatter::UrlQueryExcludeRule {
				host_regex: Regex::new(r"^(?:www\.)?instagram\.com$")
					.expect("valid Instagram host regex"),
				exclude: ["igsh".into()].into(),
			},
		];
		// If every rule reports `Skipped`, the message
		// does not require formatting.
		rules.iter().fold(true, |acc, rule| {
			acc & matches!(
				rule.apply(&mut utf16_text, &mut entities),
				formatter::Effect::Skipped
			)
		})
	} {
		return Ok(None);
	}

	// Convert UTF-16 buffer back to UTF-8 as required by Telegram.
	let mut utf8_text = String::from_utf16_lossy(&utf16_text);
	if quote {
		// Append the original author's mention as a footer.
		let Some(mention) = sender_mention(msg) else {
			log::error!(
				"Failed to obtain sender mention: MessageId={}|ChatId={}",
				msg.id,
				msg.chat.id
			);
			return Ok(None);
		};
		if let Err(err) = write!(&mut utf8_text, "\n— {}", mention) {
			log::error!(
				"Failed to write sender mention to message footer: {}",
				err
			);
			return Ok(None);
		}
		// Wrap the entire message in a blockquote for visual distinction.
		entities.push(MessageEntity::new(
			MessageEntityKind::Blockquote,
			0,
			utf16_text.len(),
		));
	}

	// Send the formatted replacement message.
	let mut send_message = bot
		.send_message(msg.chat.id, utf8_text)
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
	// Remove the original unformatted message.
	bot.delete_message(msg.chat.id, msg.id).await?;
	Ok(Some(send_message.await?))
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

/// Returns a HTML representation of the `msg` with all necessary formatting
/// to forward it to a user private messages.
fn build_message_forward(msg: &Message) -> Result<String, ()> {
	let Some(mention) = sender_mention(msg) else {
		return Err(());
	};
	Ok(format!(
		"<blockquote>{}</blockquote>\n— {}",
		Renderer::new(
			msg.text().unwrap_or_default(),
			msg.entities().unwrap_or_default(),
		)
		.as_html(),
		mention
	))
}

async fn handle_supergroup_message(
	bot: Bot,
	msg: Message,
	sqlite_pool: Pool<ConnectionManager<SqliteConnection>>,
) -> ResponseResult<()> {
	let mut sqlite_conn = match sqlite_pool.get() {
		Err(err) => {
			log::error!("Failed to get SQLite connection: {}", err);
			return Ok(());
		}
		Ok(conn) => conn,
	};

	let maybe_formatted_msg = maybe_format_message(&bot, &msg, true).await?;
	// If the message was formatted.
	if let Some(formatted_msg) = &maybe_formatted_msg {
		// Store the formatted message to support reply notifications later.
		if let Err(err) = diesel::insert_into(formatted_messages::table)
			.values(NewFormattedMessage {
				message_id: formatted_msg.id.0,
				user_tgid: msg.from.as_ref().map_or_else(
					|| {
						log::warn!(
							"Invalid empty UserId assigned to MessageId\
							={}|ChatId={}",
							formatted_msg.id.0,
							formatted_msg.chat.id.0
						);
						0
					},
					|user| user.id.0 as i64,
				),
				chat_tgid: formatted_msg.chat.id.0,
			})
			.execute(&mut sqlite_conn)
		{
			log::error!("Failed to insert formatted message: {}", err);
		}
	}
	// Skip further notification processing if the message isn't a reply.
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
				"Failed to select formatted message: MessageId={}|ChatId={} \
				err={}",
				replied_msg.id.0,
				replied_msg.chat.id.0,
				err
			);
			None
		}
		Ok(ok) => ok,
	};
	// If the message isn't found, it means the reply wasn't sent by a bot.
	// Telegram notifications will take care of that.
	let Some(user_id) = maybe_user_id else {
		return Ok(());
	};
	// Avoid notifying authors of their own replies.
	if user_id == msg.from.as_ref().map_or(0, |user| user.id.0 as i64) {
		return Ok(());
	}

	// Use either the formatted message or, if it hasn't been modified,
	// use the original.
	let effective_msg = match &maybe_formatted_msg {
		None => &msg,
		Some(formatted) => formatted,
	};
	let Some(msg_link) = message_link(&effective_msg) else {
		log::error!(
			"Failed to obtain message link: MessageId={}|ChatId={}",
			msg.id,
			msg.chat.id
		);
		return Ok(());
	};
	let Ok(reply_forward) = build_message_forward(&effective_msg) else {
		log::error!(
			"Failed to build reply forward: MessageId={}|ChatId={}",
			effective_msg.id,
			effective_msg.chat.id,
		);
		return Ok(());
	};
	// Notify the original author about the reply.
	let formatted_reply = bot
		.send_message(UserId(user_id as u64), reply_forward)
		.parse_mode(ParseMode::Html)
		.link_preview_options(LinkPreviewOptions {
			is_disabled: true,
			url: None,
			prefer_small_media: false,
			prefer_large_media: false,
			show_above_text: false,
		})
		.reply_parameters(ReplyParameters {
			message_id: replied_msg.id,
			chat_id: Some(Recipient::Id(replied_msg.chat.id)),
			..ReplyParameters::default()
		})
		.reply_markup(InlineKeyboardMarkup::new(vec![vec![
			InlineKeyboardButton::url("View in chat", msg_link),
		]]))
		.await?;
	// Store the formatted reply to support reply editing later.
	if let Err(err) = diesel::insert_into(formatted_replies::table)
		.values(NewFormattedReply {
			message_id: formatted_reply.id.0,
			user_tgid: user_id,
			source_message_id: effective_msg.id.0,
			source_chat_tgid: effective_msg.chat.id.0,
		})
		.execute(&mut sqlite_conn)
	{
		log::error!("Failed to insert formatted reply: {}", err);
	}
	Ok(())
}

async fn handle_supergroup_edited_message(
	bot: Bot,
	standalone_or_reply: Message,
	sqlite_pool: Pool<ConnectionManager<SqliteConnection>>,
) -> ResponseResult<()> {
	// Look up whether the edited message was reply to the formatted message.
	let mut sqlite_conn = match sqlite_pool.get() {
		Err(err) => {
			log::error!("Failed to get SQLite connection: {}", err);
			return Ok(());
		}
		Ok(conn) => conn,
	};
	let maybe_reply = match formatted_replies::table
		.filter(
			formatted_replies::source_message_id.eq(standalone_or_reply.id.0),
		)
		.filter(
			formatted_replies::source_chat_tgid
				.eq(standalone_or_reply.chat.id.0),
		)
		.select((formatted_replies::message_id, formatted_replies::user_tgid))
		.first::<(i32, i64)>(&mut sqlite_conn)
		.optional()
	{
		Err(err) => {
			log::error!(
				"Failed to select formatted reply: MessageId={}|ChatId={} \
				err={}",
				standalone_or_reply.id.0,
				standalone_or_reply.chat.id.0,
				err
			);
			None
		}
		Ok(ok) => ok,
	};
	let Some((message_id, user_tgid)) = maybe_reply else {
		return Ok(());
	};

	let Ok(reply_forward) = build_message_forward(&standalone_or_reply) else {
		log::error!(
			"Failed to build reply forward: MessageId={}|ChatId={}",
			standalone_or_reply.id,
			standalone_or_reply.chat.id,
		);
		return Ok(());
	};
	let Some(msg_link) = message_link(&standalone_or_reply) else {
		log::error!(
			"Failed to obtain message link: MessageId={}|ChatId={}",
			standalone_or_reply.id,
			standalone_or_reply.chat.id
		);
		return Ok(());
	};
	bot.edit_message_text(
		UserId(user_tgid as u64),
		MessageId(message_id),
		reply_forward,
	)
	.parse_mode(ParseMode::Html)
	.link_preview_options(LinkPreviewOptions {
		is_disabled: true,
		url: None,
		prefer_small_media: false,
		prefer_large_media: false,
		show_above_text: false,
	})
	.reply_markup(InlineKeyboardMarkup::new(vec![vec![
		InlineKeyboardButton::url("View in chat", msg_link),
	]]))
	.await?;
	Ok(())
}

async fn handle_private_message(
	bot: Bot,
	msg: Message,
	_: Pool<ConnectionManager<SqliteConnection>>,
) -> ResponseResult<()> {
	maybe_format_message(&bot, &msg, false).await?;
	Ok(())
}

#[tokio::main]
async fn main() {
	env_logger::init();
	let sqlite_pool = Pool::builder()
		.max_size(8)
		.build(ConnectionManager::<SqliteConnection>::new(
			env::var("MAMKILOVER_SQLITE_PATH")
				.expect("path to SQLite database file"),
		))
		.unwrap();
	Dispatcher::builder(
		Bot::from_env(),
		dptree::entry()
			.branch(
				Update::filter_message()
					.filter(|msg: Message| msg.chat.is_supergroup())
					.endpoint(handle_supergroup_message),
			)
			.branch(
				Update::filter_edited_message()
					.filter(|msg: Message| msg.chat.is_supergroup())
					.endpoint(handle_supergroup_edited_message),
			)
			.branch(
				Update::filter_message()
					.filter(|msg: Message| msg.chat.is_private())
					.endpoint(handle_private_message),
			),
	)
	.dependencies(dptree::deps![sqlite_pool])
	.enable_ctrlc_handler()
	.build()
	.dispatch()
	.await;
}

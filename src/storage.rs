use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::openai::ChatTurn;

const DEFAULT_CHAT_TITLE: &str = "New chat";
const CHAT_TOML: &str = "chat.toml";
const CONVERSATION_MD: &str = "conversation.md";
const MESSAGE_END_MARKER: &str = "<!-- /message -->";

#[derive(Debug, Clone)]
pub struct ChatStore {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StoredChat {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub turns: Vec<ChatTurn>,
}

#[derive(Debug, Clone)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMetadata {
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    model: String,
}

impl ChatStore {
    pub fn new(config: &AppConfig) -> Result<Self> {
        fs::create_dir_all(&config.chats_dir)
            .with_context(|| format!("failed to create {}", config.chats_dir.display()))?;
        Ok(Self {
            root: config.chats_dir.clone(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_chats(&self) -> Result<Vec<ChatSummary>> {
        let mut chats = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to read {}", self.root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            if let Ok(chat) = self.load_chat_from_dir(entry.path()) {
                chats.push(ChatSummary {
                    id: chat.id,
                    title: chat.title,
                    created_at: chat.created_at,
                    updated_at: chat.updated_at,
                    model: chat.model,
                });
            }
        }

        chats.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(chats)
    }

    pub fn create_chat(&self, model: &str) -> Result<StoredChat> {
        let now = Utc::now();
        let id = generate_chat_id(now);
        let chat = StoredChat {
            id,
            title: DEFAULT_CHAT_TITLE.to_string(),
            created_at: now,
            updated_at: now,
            model: model.to_string(),
            turns: Vec::new(),
        };
        self.save_chat(&chat)?;
        Ok(chat)
    }

    pub fn load_chat(&self, id: &str) -> Result<StoredChat> {
        self.load_chat_from_dir(self.root.join(id))
    }

    pub fn save_chat(&self, chat: &StoredChat) -> Result<()> {
        let chat_dir = self.root.join(&chat.id);
        fs::create_dir_all(&chat_dir)
            .with_context(|| format!("failed to create {}", chat_dir.display()))?;

        let metadata = ChatMetadata {
            title: chat.title.clone(),
            created_at: chat.created_at,
            updated_at: chat.updated_at,
            model: chat.model.clone(),
        };
        let metadata_path = chat_dir.join(CHAT_TOML);
        let metadata_raw =
            toml::to_string_pretty(&metadata).context("failed to serialize chat metadata")?;
        fs::write(&metadata_path, metadata_raw)
            .with_context(|| format!("failed to write {}", metadata_path.display()))?;

        let conversation_path = chat_dir.join(CONVERSATION_MD);
        let conversation = render_conversation_markdown(chat);
        fs::write(&conversation_path, conversation)
            .with_context(|| format!("failed to write {}", conversation_path.display()))?;

        Ok(())
    }

    fn load_chat_from_dir(&self, chat_dir: PathBuf) -> Result<StoredChat> {
        let metadata_path = chat_dir.join(CHAT_TOML);
        let metadata_raw = fs::read_to_string(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        let metadata: ChatMetadata = toml::from_str(&metadata_raw)
            .with_context(|| format!("failed to parse {}", metadata_path.display()))?;

        let conversation_path = chat_dir.join(CONVERSATION_MD);
        let turns = if conversation_path.exists() {
            let raw = fs::read_to_string(&conversation_path)
                .with_context(|| format!("failed to read {}", conversation_path.display()))?;
            parse_conversation_markdown(&raw)
        } else {
            Vec::new()
        };

        let id = chat_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("invalid chat directory name: {}", chat_dir.display()))?
            .to_string();

        Ok(StoredChat {
            id,
            title: metadata.title,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            model: metadata.model,
            turns,
        })
    }
}

pub fn refresh_chat_metadata(chat: &mut StoredChat) {
    chat.updated_at = Utc::now();
    if chat.title == DEFAULT_CHAT_TITLE {
        if let Some(title) = derive_title(&chat.turns) {
            chat.title = title;
        }
    }
}

fn generate_chat_id(now: DateTime<Utc>) -> String {
    let mut rng = rand::rng();
    let suffix: u32 = rng.random();
    format!("{}_{}", now.format("%Y-%m-%d_%H-%M-%S"), format!("{suffix:08x}"))
}

fn derive_title(turns: &[ChatTurn]) -> Option<String> {
    let first_user = turns.iter().find(|turn| turn.role == "user")?;
    let collapsed = first_user
        .content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }

    let mut title = collapsed.chars().take(60).collect::<String>();
    if collapsed.chars().count() > 60 {
        title.push_str("...");
    }
    Some(title)
}

fn render_conversation_markdown(chat: &StoredChat) -> String {
    let mut output = format!("# {}\n\n", chat.title);
    for turn in &chat.turns {
        let role = match turn.role.as_str() {
            "assistant" => "assistant",
            _ => "user",
        };
        output.push_str(&format!("<!-- role: {role} -->\n"));
        output.push_str(turn.content.trim_end());
        output.push('\n');
        output.push_str(MESSAGE_END_MARKER);
        output.push_str("\n\n");
    }
    output
}

fn parse_conversation_markdown(raw: &str) -> Vec<ChatTurn> {
    let mut turns = Vec::new();
    let mut current_role: Option<String> = None;
    let mut buffer = Vec::new();

    for line in raw.lines() {
        if let Some(role) = parse_role_marker(line) {
            current_role = Some(role.to_string());
            buffer.clear();
            continue;
        }

        if line.trim() == MESSAGE_END_MARKER {
            if let Some(role) = current_role.take() {
                let content = buffer.join("\n").trim().to_string();
                if !content.is_empty() {
                    turns.push(ChatTurn { role, content });
                }
            }
            buffer.clear();
            continue;
        }

        if current_role.is_some() {
            buffer.push(line.to_string());
        }
    }

    turns
}

fn parse_role_marker(line: &str) -> Option<&str> {
    match line.trim() {
        "<!-- role: user -->" => Some("user"),
        "<!-- role: assistant -->" => Some("assistant"),
        _ => None,
    }
}

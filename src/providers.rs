//! Provider and model initialization.
//!
//! Every provider is reduced to one erased `DynModel<Completion>`, so the rest
//! of the application does not know or care which vendor is behind it.

use anyhow::{Context, Result};
use rig::operation::Completion;
use rig::prelude::*;
use rig::providers::{deepseek, ollama, openai};

use crate::config::{Config, Provider};

/// Build the completion model described by `config`.
///
/// Credentials come from [`Config`], which read them from the environment; they
/// are passed straight to the provider client and never logged.
pub fn build_model(config: &Config) -> Result<DynModel<Completion>> {
    let model = match config.provider {
        Provider::Openai => {
            let key = config
                .api_key
                .as_deref()
                .context("OpenAI requires an API key")?;
            openai::OpenAI::new(key).completion(&config.model).erase()
        }
        Provider::Deepseek => {
            let key = config
                .api_key
                .as_deref()
                .context("DeepSeek requires an API key")?;
            // `deepseek::new` returns an OpenAI-shaped client on the DeepSeek
            // dialect (base URL, model names and quirks included).
            deepseek::new(key).completion(&config.model).erase()
        }
        Provider::Ollama => {
            let mut daemon = ollama::OllamaConfig::new().with_base_url(&config.ollama_base_url);
            if let Some(token) = config.api_key.as_deref() {
                daemon = daemon.with_api_key(token);
            }
            // The daemon's OpenAI-compatible chat route, which supports tools
            // and streaming for models that implement them.
            daemon.client().completion(&config.model).erase()
        }
    };

    tracing::info!(
        provider = config.provider.label(),
        model = %config.model,
        "model initialized"
    );
    Ok(model)
}

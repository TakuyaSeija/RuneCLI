//! Everything the user configures: providers, keys, and per-session model knobs.
//!
//! Two files under `~/.rune/`, each with one job and different rules:
//!
//! | file | holds | if it is broken |
//! |---|---|---|
//! | `rune.toml` ([`configuration`]) | engine limits, endpoints and models | named error, the engine refuses to start |
//! | `secrets.vault` ([`secrets`], [`vault`]) | API keys, encrypted, `0600` | named error, nothing starts |
//!
//! The split is not tidiness. Limits are enforced by the engine, so a value the
//! user believes is in force and which is silently ignored is a safety problem —
//! hence refuse to start. Keys are credentials, so they get their own file with
//! its own permissions and never share a parser with anything that might print
//! its input.

#[path = "Configuration.rs"]
pub mod configuration;
pub mod connections;
pub mod manage;
pub mod model;
pub mod providers;
pub mod secrets;
pub mod services;
pub mod vault;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub use model::{Knob, KnobKind, ModelSettings, KNOBS};
pub use providers::{ProviderConfig, Providers};
pub use secrets::Secrets;
pub use services::Services;

/// The user's configuration, loaded together because every part of it is needed
/// to answer one question: can this session talk to a model, and how.
pub struct Settings {
    pub home: PathBuf,
    /// The one hot-reloadable configuration, shared with `crate::limits::Limits`
    /// and every consumer. A `SettingsSet` swaps it atomically; readers see the
    /// new value on their next read. Never a by-value snapshot.
    pub config: configuration::Shared,
    pub providers: Providers,
    pub secrets: Secrets,
    /// The connections that serve the tools (web search, page fetching).
    pub services: Services,
}

impl Settings {
    /// The settings in `home`, with the providers' model catalogue kept in
    /// the store's database at `db` — named, never assumed to sit beside
    /// `rune.toml`.
    pub fn load(home: &Path, db: &Path) -> Result<Settings> {
        let config = configuration::Shared::load(home)?;
        let catalogue = crate::store::catalogue::Catalogue::at(db, config.clone());
        Ok(Settings {
            home: home.to_path_buf(),
            providers: Providers::load(home, catalogue)?,
            services: Services::load(home)?,
            config,
            secrets: Secrets::load(home)?,
        })
    }

    /// Re-read the credential store from disk.
    ///
    /// A key is added by whichever process the user happened to be in — quite
    /// often a second terminal running `rune -provider key` — and that process
    /// writes the vault and exits. A long-lived engine therefore holds a copy
    /// of the store that stopped being true the moment the other process
    /// exited. Called when a session is opened and before every turn.
    ///
    /// The store on disk is the truth (owner, 2026-10-16): a key added,
    /// removed, or lost with its file — in another process or by hand — is
    /// what this process uses and shows next, never a copy it still held. A
    /// store that cannot be opened is unavailable here too, with its cause;
    /// keys once read stay registered for redaction, so none can surface.
    pub fn reload_secrets(&mut self) {
        if self.home.as_os_str().is_empty() {
            return;
        }
        if let Ok(fresh) = Secrets::load(&self.home) {
            self.secrets = fresh;
        }
    }

    /// Re-read the provider list and cached model metadata from disk, ensuring
    /// changes from other processes (such as `rune -provider add`) are immediately
    /// available in real time without restarting the engine.
    ///
    /// What disk holds is taken whole — a list emptied by another process is
    /// empty here too; a file that does not read leaves the list as it was. A
    /// configuration with no home has nothing on disk to follow.
    pub fn reload_providers(&mut self) {
        if self.home.as_os_str().is_empty() {
            return;
        }
        if let Ok(fresh) = Providers::load(&self.home, self.providers.catalogue().clone()) {
            self.providers = fresh;
        }
        if let Ok(fresh) = Services::load(&self.home) {
            self.services = fresh;
        }
    }

    /// Bring the service connections and the keys up to what disk holds when
    /// the configuration moved since they were read — what a web tool does
    /// before it picks who serves, so a connection or key another process
    /// saved serves this search, not the one after the next command.
    pub fn follow_services(&mut self) {
        let moved = self.config.generation() != self.services.seen();
        self.services.follow(&self.config);
        if moved && !self.home.as_os_str().is_empty() {
            self.reload_secrets();
        }
    }

    /// A configuration with nothing in it and nowhere to save to.
    ///
    /// For tests and for the read-only paths that need the type but no user
    /// state. It cannot be written back, on purpose: an empty configuration that
    /// silently saved itself would overwrite a real one.
    pub fn empty() -> Settings {
        Settings {
            home: PathBuf::new(),
            config: configuration::Shared::default(),
            providers: Providers::default(),
            secrets: Secrets::default(),
            services: Services::default(),
        }
    }

    /// The key for `provider`, or a message that says exactly what to do.
    ///
    /// A provider with no `secret` needs none — a local Ollama — and that is a
    /// success, not an empty string standing in for one.
    pub fn key_for(&self, p: &ProviderConfig) -> Result<Option<String>> {
        let Some(name) = &p.secret else {
            return Ok(None);
        };
        // The same sentence Settings shows: missing, or the store unavailable.
        match self.secrets.get(name) {
            Some(k) => Ok(Some(k.to_string())),
            None => Err(Error::invalid(format!(
                "{} — add a key for `{}` in Settings › Connections or with `rune -provider key {} <key>`",
                self.secrets.key_state(name).note(name),
                p.name,
                p.name
            ))),
        }
    }

    /// Ready-to-send headers for one provider: content type plus the credential.
    pub fn headers_for(&self, p: &ProviderConfig) -> Result<Vec<(String, String)>> {
        let mut out = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(key) = self.key_for(p)? {
            let (header, val) = if p.base_url.contains("generativelanguage.googleapis.com")
                && p.base_url.contains("/openai")
            {
                ("Authorization".to_string(), format!("Bearer {key}"))
            } else {
                (p.auth.header.clone(), format!("{}{key}", p.auth.prefix))
            };
            out.push((header, val));
        }
        // Token-saving routers (9Router's RTK) rewrite long tool results into
        // summaries — a numbered file read once reached the model as
        // "clean — nothing to commit". The default opts every request out;
        // handing the compression back is `network.token_saver = "on"`.
        // No third-party API exists to discover the header name — hardcoded as
        // of 2026-09. Before the provider's own list, so it too cannot be
        // overwritten into a mangled transcript by a stray config line.
        if self.config.text("network.token_saver") != "on" {
            out.push(("x-9router-token-saver".to_string(), "off".to_string()));
        }
        // After auth, so a provider's own header list can never overwrite the
        // credential by naming the same header.
        for (name, value) in &p.headers {
            if !out
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
            {
                let value = if let Some(reference) = value.strip_prefix("@secret:") {
                    self.secrets
                        .get(reference)
                        .ok_or_else(|| {
                            Error::invalid(format!(
                                "header {name} names a credential: {}",
                                self.secrets.key_state(reference).note(reference)
                            ))
                        })?
                        .to_string()
                } else {
                    value.clone()
                };
                out.push((name.clone(), value));
            }
        }
        Ok(out)
    }

    /// Settings for a brand-new session, from whichever provider is active.
    ///
    /// `None` when nothing is configured yet: that is the first-run state, and
    /// the frontend answers it by running the add-provider flow rather than by
    /// inventing a default that cannot work.
    pub fn defaults(&self) -> Option<ModelSettings> {
        self.providers.active().map(ModelSettings::for_provider)
    }

    /// Price of a request based on configured provider and model rates.
    pub fn price_of(
        &self,
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
        cached: u64,
        details: &crate::provider::UsageDetails,
    ) -> Option<f64> {
        if let Some(cost) = details.provider_cost {
            return Some(cost);
        }
        if !details.measured || !details.input_known || !details.output_known {
            return None;
        }
        let price = self
            .providers
            .get(provider)
            .and_then(|p| p.model(model))
            .and_then(|m| m.price.as_ref());
        match price {
            Some(p) => p.cost(input, output, cached, details),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Owner, 2026-10-16: a damaged credential store refused every start —
    /// Settings, where it would be put right, included.
    #[test]
    fn a_store_that_cannot_be_opened_never_stops_a_start_and_says_why_a_key_is_missing() {
        let h = home("damaged-store");
        std::fs::write(h.join(secrets::VAULT_FILE), b"not a sealed store at all").unwrap();
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("mimo").unwrap().build("sk-x"))
            .unwrap();
        let e = s
            .headers_for(s.providers.active().unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("Key unavailable"), "{e}");
        assert_eq!(
            std::fs::read(h.join(secrets::VAULT_FILE)).unwrap(),
            b"not a sealed store at all",
            "left exactly as it was"
        );
    }

    #[test]
    fn a_fresh_install_has_no_defaults_rather_than_a_broken_one() {
        let h = home("fresh");
        let s = Settings::load(&h, &h.join("rune.db")).unwrap();
        assert!(s.defaults().is_none());
    }

    #[test]
    fn a_provider_without_its_key_says_what_to_run() {
        let h = home("nokey");
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("mimo").unwrap().build("sk-x"))
            .unwrap();
        let e = s
            .headers_for(s.providers.active().unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("rune -provider key mimo"), "{e}");
        assert!(e.contains("Key missing"), "{e}");
        assert!(!e.contains("invalid: invalid"), "{e}");
    }

    #[test]
    fn headers_carry_the_credential_in_the_shape_the_provider_wants() {
        let h = home("hdr");
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("mimo").unwrap().build("sk-x"))
            .unwrap();
        s.secrets.set("mimo", "sk-header-shape-test-1").unwrap();
        let headers = s.headers_for(s.providers.active().unwrap()).unwrap();
        assert!(headers
            .iter()
            .any(|(k, v)| k == "api-key" && v == "sk-header-shape-test-1"));

        let mut openai = providers::preset("openai").unwrap().build("sk-x");
        openai.name = "oa".into();
        openai.secret = Some("oa".into());
        s.providers.add(openai).unwrap();
        s.secrets.set("oa", "sk-bearer-shape-test-1").unwrap();
        let headers = s.headers_for(s.providers.get("oa").unwrap()).unwrap();
        assert!(headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sk-bearer-shape-test-1"));
    }

    #[test]
    fn a_keyless_provider_needs_no_key_and_that_is_not_an_error() {
        let h = home("keyless");
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("ollama").unwrap().build(""))
            .unwrap();
        let headers = s.headers_for(s.providers.active().unwrap()).unwrap();
        assert_eq!(
            headers.len(),
            2,
            "content-type plus the token-saver opt-out that rides even without a key"
        );
    }

    /// The default keeps token-saving routers' hands off tool results — 9Router's
    /// RTK once turned a numbered file read into "clean — nothing to commit" on
    /// its way to the model. Opting in withdraws the header; the router then does
    /// what its own settings say.
    #[test]
    fn token_saver_defaults_off_and_the_owner_can_hand_compression_back() {
        let h = home("saver");
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("ollama").unwrap().build(""))
            .unwrap();
        let headers = s.headers_for(s.providers.active().unwrap()).unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-9router-token-saver" && v == "off"),
            "the default must opt the request out of router-side compression: {headers:?}"
        );

        let mut toml = h.clone();
        toml.push("rune.toml");
        std::fs::write(&toml, "[network]\ntoken_saver = \"on\"\n").unwrap();
        let mut s = Settings::load(&h, &h.join("rune.db")).unwrap();
        s.providers
            .add(providers::preset("ollama").unwrap().build(""))
            .unwrap();
        let headers = s.headers_for(s.providers.active().unwrap()).unwrap();
        assert!(
            !headers.iter().any(|(k, _)| k == "x-9router-token-saver"),
            "an owner who opts in gets no opt-out header: {headers:?}"
        );
    }
}

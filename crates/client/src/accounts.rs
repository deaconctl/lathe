use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use futures::AsyncReadExt as _;
use gpui::AsyncApp;
use http_client::{AsyncBody, HttpClient as _};
use util::ResultExt as _;

use crate::Client;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollabAccount {
    pub id: String,
    pub label: String,
    pub user_id: u64,
    pub server_url: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub avatar_uri: Option<String>,
    /// GitHub's numeric id for `login`. `user_id` is Zed's own id, which
    /// GitHub can't use to attribute commits.
    #[serde(default)]
    pub github_user_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollabAccountsIndex {
    #[serde(default)]
    pub accounts: Vec<CollabAccount>,
    #[serde(default)]
    pub active_id: Option<String>,
}

impl CollabAccountsIndex {
    pub fn active(&self) -> Option<&CollabAccount> {
        let id = self.active_id.as_ref()?;
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn find(&self, id: &str) -> Option<&CollabAccount> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn upsert(&mut self, account: CollabAccount) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<CollabAccount> {
        let position = self.accounts.iter().position(|a| a.id == id)?;
        let removed = self.accounts.remove(position);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = self.accounts.first().map(|a| a.id.clone());
        }
        Some(removed)
    }
}

fn accounts_path() -> PathBuf {
    paths::account_indexes_dir().join("collab_accounts.json")
}

/// The parsed index, keyed by the file's modification time. The title bar reads
/// the index on every render, so this turns a read and JSON parse into a `stat`
/// while still picking up writes from other Lathe instances.
static INDEX_CACHE: Mutex<Option<(SystemTime, CollabAccountsIndex)>> = Mutex::new(None);

static ADDING_ACCOUNT: AtomicBool = AtomicBool::new(false);

pub fn load_index() -> CollabAccountsIndex {
    let path = accounts_path();
    let Some(modified) = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
    else {
        *INDEX_CACHE.lock() = None;
        return CollabAccountsIndex::default();
    };
    let mut cache = INDEX_CACHE.lock();
    if let Some((cached_modified, index)) = cache.as_ref()
        && *cached_modified == modified
    {
        return index.clone();
    }
    let index: CollabAccountsIndex = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    *cache = Some((modified, index.clone()));
    index
}

pub fn save_index(index: &CollabAccountsIndex) -> Result<()> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(index).context("serialize collab accounts index")?;
    *INDEX_CACHE.lock() = None;
    paths::write_account_index(&path, &bytes).context("write collab_accounts.json")
}

/// Caches the login + avatar of the account signed in as `user_id` so other
/// workspaces bound to it can render it in their title bars even when it isn't
/// the currently-connected account. Keyed by user id rather than the index's
/// active account because another Lathe instance may have changed that.
pub fn set_user_info(user_id: u64, login: &str, avatar_uri: &str) -> Result<()> {
    let mut index = load_index();
    let Some(account) = index.accounts.iter_mut().find(|a| a.user_id == user_id) else {
        return Ok(());
    };
    let mut changed = false;
    if account.label != login {
        account.label = login.to_string();
        changed = true;
    }
    if account.login.as_deref() != Some(login) {
        account.login = Some(login.to_string());
        account.github_user_id = None;
        changed = true;
    }
    if account.avatar_uri.as_deref() != Some(avatar_uri) {
        account.avatar_uri = Some(avatar_uri.to_string());
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    save_index(&index)
}

pub fn account_id_for(server_url: &str, user_id: u64) -> String {
    let host = url::Url::parse(server_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| server_url.to_string());
    format!("{host}#{user_id}")
}

pub fn keychain_url(server_url: &str, user_id: u64) -> String {
    format!("{server_url}#user_id={user_id}")
}

/// Multi-account collab session management. Lathe lets a user keep several
/// signed-in collab accounts and switch between them; these methods drive the
/// disconnect/reauth handshake and keep the account index in sync with the
/// active session. Kept here, alongside the account persistence layer, so the
/// multi-account feature stays out of the upstream-owned `client.rs`.
impl Client {
    /// Returns all saved collab accounts.
    pub fn list_accounts(&self) -> Vec<CollabAccount> {
        load_index().accounts
    }

    /// Returns the id of the saved account this process is signed in as,
    /// falling back to the index's active account while signed out. The index
    /// is shared with other Lathe instances, so its active account can belong
    /// to another process's session.
    pub fn active_account_id(&self) -> Option<String> {
        let index = load_index();
        if !self.status().borrow().is_signed_out()
            && let Some(user_id) = self.user_id()
            && let Some(account) = index.accounts.iter().find(|a| a.user_id == user_id)
        {
            return Some(account.id.clone());
        }
        index.active_id
    }

    /// Looks up and caches the GitHub id for the saved account signed in as
    /// `user_id`, which commit attribution needs and Zed doesn't provide.
    pub fn refresh_github_user_id(self: &Arc<Self>, user_id: u64, login: String, cx: &AsyncApp) {
        let already_known = load_index().accounts.iter().any(|account| {
            account.user_id == user_id
                && account.login.as_deref() == Some(login.as_str())
                && account.github_user_id.is_some()
        });
        if already_known
            || login.is_empty()
            || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return;
        }

        let http = self.http.clone();
        let fetch = async move {
            #[derive(Deserialize)]
            struct GithubUser {
                id: u64,
            }

            let url = format!("https://api.github.com/users/{login}");
            let mut response = http
                .get(&url, AsyncBody::default(), true)
                .await
                .with_context(|| format!("fetching {url}"))?;
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;
            anyhow::ensure!(
                response.status().is_success(),
                "fetching {url}: status {}",
                response.status()
            );
            let github_user: GithubUser =
                serde_json::from_slice(&body).context("parsing GitHub user")?;

            let mut index = load_index();
            let Some(account) = index.accounts.iter_mut().find(|account| {
                account.user_id == user_id && account.login.as_deref() == Some(login.as_str())
            }) else {
                return Ok(());
            };
            account.github_user_id = Some(github_user.id);
            save_index(&index)
        };
        cx.background_executor()
            .spawn(async move {
                fetch.await.log_err();
            })
            .detach();
    }

    /// Disconnects the current session, marks the given account as active,
    /// and signs back in using that account's stored credentials.
    pub async fn switch_account(self: &Arc<Self>, account_id: String, cx: &AsyncApp) -> Result<()> {
        let mut index = load_index();
        if index.find(&account_id).is_none() {
            anyhow::bail!("unknown collab account: {account_id}");
        }
        if self.active_account_id().as_deref() == Some(&account_id)
            && !self.status().borrow().is_signed_out()
        {
            return Ok(());
        }

        self.state.write().credentials = None;
        self.cloud_client.clear_credentials();
        self.disconnect(cx);

        index.active_id = Some(account_id);
        save_index(&index).log_err();

        self.sign_in_with_optional_connect(true, cx).await
    }

    /// Whether an add-account sign-in is in flight. Workspaces bound to an
    /// account must not switch to it meanwhile: the switch would cancel the
    /// browser sign-in and sign the old account back in.
    pub fn is_adding_account(&self) -> bool {
        ADDING_ACCOUNT.load(Ordering::SeqCst)
    }

    /// Starts a new browser sign-in flow to add a second account, disconnecting
    /// the current session first. The new account is stored under its own
    /// keychain entry and becomes the active account on success. Returns the
    /// new account's id.
    pub async fn add_account(self: &Arc<Self>, cx: &AsyncApp) -> Result<String> {
        ADDING_ACCOUNT.store(true, Ordering::SeqCst);
        let _clear_adding_account = util::defer(|| ADDING_ACCOUNT.store(false, Ordering::SeqCst));

        self.state.write().credentials = None;
        self.cloud_client.clear_credentials();
        self.disconnect(cx);

        // Clearing active_id causes read_credentials to return None, which
        // forces a fresh browser auth flow. write_credentials will insert the
        // new account into the index and mark it active.
        let mut index = load_index();
        index.active_id = None;
        save_index(&index).log_err();

        self.sign_in_with_optional_connect(true, cx).await?;
        load_index()
            .active_id
            .context("sign-in finished without an active collab account")
    }

    /// Deletes the given account's keychain entry and removes it from the
    /// index. If it was the active account, the session is torn down; if other
    /// accounts remain, the first is promoted and signed in.
    pub async fn remove_account(self: &Arc<Self>, account_id: String, cx: &AsyncApp) -> Result<()> {
        let mut index = load_index();
        let Some(account) = index.find(&account_id).cloned() else {
            return Ok(());
        };

        let was_active = index.active_id.as_deref() == Some(&account_id);
        let server_url = self
            .credentials_provider
            .server_url(cx)
            .unwrap_or_else(|_| account.server_url.clone());
        let keychain_url = keychain_url(&server_url, account.user_id);

        if was_active {
            self.state.write().credentials = None;
            self.cloud_client.clear_credentials();
            self.disconnect(cx);
        }

        self.credentials_provider
            .provider
            .delete_credentials(&keychain_url, cx)
            .await
            .log_err();

        index.remove(&account_id);
        save_index(&index).log_err();

        if was_active && !index.accounts.is_empty() {
            self.sign_in_with_optional_connect(true, cx).await?;
        }
        Ok(())
    }
}

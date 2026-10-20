use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Context, EntityId, Global, Subscription,
    Task,
};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::hosting_provider::{
    GitHostAuth, GitHostAuthKind, GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote,
    RepositoryAccess,
};

/// A specific git host Lathe can authenticate against for pull-request
/// operations: the protocol it speaks plus the hostname it lives at.
///
/// Resolved from the hosting-provider registry rather than hardcoded, so
/// enterprise and self-hosted instances are first-class. Their hostnames are
/// only known at runtime (from the `git_hosting_providers` setting, or inferred
/// from the repository's own remote), which a fixed enum could never express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHost {
    kind: GitHostAuthKind,
    host: Arc<str>,
    /// The provider's configured name, e.g. "GitHub" or "BigCorp GitHub".
    display_name: Arc<str>,
}

impl GitHost {
    pub fn kind(&self) -> GitHostAuthKind {
        self.kind
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Name to show in menus and modals. Falls back to the hostname when a
    /// self-hosted provider was configured without a distinguishing name, so
    /// two GitHub entries never render identically.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Whether this is the vendor's public instance rather than an enterprise
    /// or self-hosted deployment. Drives the connect flow: only the public
    /// GitHub can use the device flow, because the OAuth app backing it is
    /// registered against `github.com`.
    pub fn is_public_instance(&self) -> bool {
        &*self.host == self.kind.public_host()
    }

    /// Builds the API auth value for a stored `(username, secret)` credential.
    pub fn auth(&self, username: String, secret: String) -> GitHostAuth {
        self.kind.auth(username, secret)
    }

    /// Resolves a hostname to a connectable host by asking the provider registry
    /// which provider serves it. Returns `None` for hosts with no registered
    /// provider, or whose provider has no authentication flow.
    pub fn resolve(cx: &App, host: &str) -> Option<GitHost> {
        connectable_hosts(cx)
            .into_iter()
            .find(|candidate| &*candidate.host == host)
    }

    /// Async-context counterpart of [`GitHost::resolve`].
    pub fn resolve_async(cx: &AsyncApp, host: &str) -> Option<GitHost> {
        cx.update(|cx| GitHost::resolve(cx, host))
    }
}

/// Every host in the registry that Lathe knows how to authenticate against,
/// deduplicated by hostname with the vendor's public instances listed first.
///
/// The set is dynamic: registering a self-hosted provider (through settings or
/// from a repository remote) makes that instance connectable without any change
/// here.
pub fn connectable_hosts(cx: &App) -> Vec<GitHost> {
    let Some(registry) = GitHostingProviderRegistry::try_global(cx) else {
        return Vec::new();
    };
    let mut hosts: Vec<GitHost> = Vec::new();
    for provider in registry.list_hosting_providers() {
        let Some(kind) = provider.auth_kind() else {
            continue;
        };
        let Some(host) = provider.base_url().host_str().map(Arc::<str>::from) else {
            continue;
        };
        if hosts.iter().any(|existing| existing.host == host) {
            continue;
        }
        let name = provider.name();
        // A self-hosted provider configured without a distinguishing name would
        // otherwise render as a second, identical "GitHub" menu entry.
        let display_name: Arc<str> = if &*host == kind.public_host() {
            Arc::from(name.as_str())
        } else {
            Arc::from(format!("{name} ({host})").as_str())
        };
        hosts.push(GitHost {
            kind,
            host,
            display_name,
        });
    }
    hosts.sort_by_key(|host| !host.is_public_instance());
    hosts
}

/// The keychain key a host's original, single credential is stored under.
/// Still used for the account that existing connection becomes, so upgrading
/// does not ask anyone to sign in again.
pub fn host_credential_url(host: &str) -> String {
    format!("https://{host}")
}

/// The id given to a connection made before a host could hold several
/// accounts. Its secret stays under [`host_credential_url`].
const LEGACY_ACCOUNT_ID: &str = "default";

fn account_credential_url(host: &str, account_id: &str) -> String {
    if account_id == LEGACY_ACCOUNT_ID {
        host_credential_url(host)
    } else {
        format!("https://{host}/lathe-git-account/{account_id}")
    }
}

/// One connected account on a git host. Holds no secret: that lives in the
/// keychain and is read only when an API call is made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHostAccount {
    pub id: String,
    /// The account's login, shown in menus. Empty when the host couldn't
    /// report it at connect time.
    pub username: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct HostAccounts {
    accounts: Vec<GitHostAccount>,
    /// Used by workspaces that haven't picked an account of their own: the
    /// account connected or switched to most recently.
    default: Option<String>,
}

impl HostAccounts {
    fn find(&self, account_id: &str) -> Option<&GitHostAccount> {
        self.accounts
            .iter()
            .find(|account| account.id == account_id)
    }

    fn default_account(&self) -> Option<&GitHostAccount> {
        self.default
            .as_deref()
            .and_then(|id| self.find(id))
            .or_else(|| self.accounts.first())
    }

    /// The account with the most access to a repository. Among equally capable
    /// accounts the workspace's own pick wins, then the host default, so a
    /// manual choice still applies wherever it works. `None` when no account's
    /// access is known or none can use the repository, leaving the choice to
    /// [`active_account`].
    fn most_capable_account(
        &self,
        access: &HashMap<String, RepositoryAccess>,
        workspace_pick: Option<&str>,
    ) -> Option<&GitHostAccount> {
        let best = self
            .accounts
            .iter()
            .filter_map(|account| access.get(&account.id))
            .max()
            .copied()?;
        if best == RepositoryAccess::None {
            return None;
        }
        let is_best = |account: &&GitHostAccount| access.get(&account.id) == Some(&best);
        workspace_pick
            .and_then(|id| self.find(id))
            .filter(is_best)
            .or_else(|| {
                self.default
                    .as_deref()
                    .and_then(|id| self.find(id))
                    .filter(is_best)
            })
            .or_else(|| self.accounts.iter().find(is_best))
    }
}

/// Which accounts each host has. Shared by every window and every running
/// instance, since it is just a list of what's in the keychain; which one a
/// workspace uses is tracked separately, per workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AccountIndex {
    #[serde(default)]
    hosts: HashMap<String, HostAccounts>,
}

fn index_path() -> PathBuf {
    paths::account_indexes_dir().join("git_host_accounts.json")
}

fn load_index() -> AccountIndex {
    match std::fs::read(index_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).log_err().unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => AccountIndex::default(),
        Err(error) => {
            log::error!("git host accounts: failed to read index: {error}");
            AccountIndex::default()
        }
    }
}

fn save_index(index: &AccountIndex) -> Result<()> {
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    paths::write_account_index(&path, &serde_json::to_vec_pretty(index)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// What each connected account can do in one repository, keyed by account id.
/// `None` when no account's access could be determined.
type AccountAccess = Option<HashMap<String, RepositoryAccess>>;

/// Detected [`AccountAccess`] per `(host, "owner/repo")`, for this session only
/// since membership changes on the host. Cleared whenever the connected
/// accounts change.
#[derive(Default)]
struct RepositoryAccessCache(HashMap<(String, String), Shared<Task<AccountAccess>>>);

impl Global for RepositoryAccessCache {}

fn repository_key(host: &str, remote: &ParsedGitRemote) -> (String, String) {
    (
        host.to_string(),
        format!("{}/{}", remote.owner, remote.repo),
    )
}

struct GlobalGitHostCredentials(Arc<dyn CredentialsProvider>);

impl Global for GlobalGitHostCredentials {}

/// In-memory copy of the account index, for synchronous use while rendering.
/// Holds NO secrets.
#[derive(Default)]
struct GitHostConnections(AccountIndex);

impl Global for GitHostConnections {}

/// The account each workspace picked for each host, keyed by the workspace
/// entity and then by hostname. A workspace with no entry for a host uses the
/// host's default account.
#[derive(Default)]
pub struct WorkspaceGitHostAccounts(HashMap<EntityId, HashMap<String, String>>);

impl Global for WorkspaceGitHostAccounts {}

impl WorkspaceGitHostAccounts {
    pub fn bindings(workspace: EntityId, cx: &App) -> HashMap<String, String> {
        cx.try_global::<Self>()
            .and_then(|this| this.0.get(&workspace).cloned())
            .unwrap_or_default()
    }

    pub fn set_all(workspace: EntityId, bindings: HashMap<String, String>, cx: &mut App) {
        cx.default_global::<Self>().0.insert(workspace, bindings);
    }

    /// Binds (`Some`) or unbinds (`None`) the workspace's account for `host`
    /// and returns the workspace's full binding map, for persisting. Views
    /// observing connections reload, since the account behind their requests
    /// may have changed.
    pub fn bind(
        workspace: EntityId,
        host: &str,
        account_id: Option<String>,
        cx: &mut App,
    ) -> HashMap<String, String> {
        let bindings = cx.default_global::<Self>().0.entry(workspace).or_default();
        match account_id {
            Some(account_id) => {
                bindings.insert(host.to_string(), account_id);
            }
            None => {
                bindings.remove(host);
            }
        }
        let bindings = bindings.clone();
        cx.update_default_global::<GitHostConnections, _>(|_, _| {});
        cx.refresh_windows();
        bindings
    }
}

/// Which workspace each project belongs to, so code that only has the project
/// (an agent being spawned) can use that workspace's account picks.
#[derive(Default)]
struct ProjectWorkspaces(HashMap<EntityId, EntityId>);

impl Global for ProjectWorkspaces {}

pub fn register_project_workspace(project: EntityId, workspace: EntityId, cx: &mut App) {
    cx.default_global::<ProjectWorkspaces>()
        .0
        .insert(project, workspace);
}

pub fn workspace_for_project(project: EntityId, cx: &App) -> Option<EntityId> {
    cx.try_global::<ProjectWorkspaces>()?
        .0
        .get(&project)
        .copied()
}

/// Installs the credentials provider used for git-host secrets and kicks off an
/// initial refresh of the connection snapshot. Call once at startup.
pub fn init(provider: Arc<dyn CredentialsProvider>, cx: &mut App) {
    cx.set_global(GlobalGitHostCredentials(provider));
    refresh_connections(cx);
}

fn provider(cx: &AsyncApp) -> Result<Arc<dyn CredentialsProvider>> {
    cx.try_read_global::<GlobalGitHostCredentials, _>(|global, _| global.0.clone())
        .context("git host credentials store is not initialized")
}

async fn read_credential(cx: &AsyncApp, url: &str) -> Result<Option<(String, String)>> {
    let provider = provider(cx)?;
    let Some((username, secret)) = provider.read_credentials(url, cx).await? else {
        return Ok(None);
    };
    let secret = String::from_utf8(secret).context("stored credential was not valid UTF-8")?;
    Ok(Some((username, secret)))
}

/// Every account connected for `host`, in the order they were added.
pub fn accounts(cx: &App, host: &str) -> Vec<GitHostAccount> {
    cx.try_global::<GitHostConnections>()
        .and_then(|connections| connections.0.hosts.get(host))
        .map(|host_accounts| host_accounts.accounts.clone())
        .unwrap_or_default()
}

/// The account `workspace` uses for `host`: the workspace's own pick when it
/// still exists, otherwise the host's default. `None` when nothing is
/// connected.
pub fn active_account(cx: &App, workspace: Option<EntityId>, host: &str) -> Option<GitHostAccount> {
    let host_accounts = cx.try_global::<GitHostConnections>()?.0.hosts.get(host)?;
    workspace_pick(cx, workspace, host)
        .and_then(|bound_id| host_accounts.find(bound_id))
        .or_else(|| host_accounts.default_account())
        .cloned()
}

fn workspace_pick<'a>(cx: &'a App, workspace: Option<EntityId>, host: &str) -> Option<&'a str> {
    cx.try_global::<WorkspaceGitHostAccounts>()?
        .0
        .get(&workspace?)?
        .get(host)
        .map(String::as_str)
}

/// The account `workspace` uses for one repository on `host`: whichever
/// connected account can do the most there, once detected, otherwise the
/// workspace's account for the host. Synchronous, for rendering; detection is
/// started by [`auth_for_repository`].
pub fn account_for_repository(
    cx: &App,
    workspace: Option<EntityId>,
    host: &str,
    remote: &ParsedGitRemote,
) -> Option<GitHostAccount> {
    let detected = cx
        .try_global::<RepositoryAccessCache>()
        .and_then(|cache| cache.0.get(&repository_key(host, remote)))
        .and_then(|detection| detection.peek())
        .and_then(Option::as_ref);
    if let Some(access) = detected
        && let Some(host_accounts) = cx
            .try_global::<GitHostConnections>()
            .and_then(|connections| connections.0.hosts.get(host))
        && let Some(account) =
            host_accounts.most_capable_account(access, workspace_pick(cx, workspace, host))
    {
        return Some(account.clone());
    }
    active_account(cx, workspace, host)
}

/// Asks the host what each connected account can do in the repository, once
/// per session. Returns `None` when there is nothing to choose between.
fn detect_repository_access(
    cx: &mut App,
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: &ParsedGitRemote,
    http_client: Arc<dyn HttpClient>,
) -> Option<Shared<Task<AccountAccess>>> {
    let host = provider.base_url().host_str()?.to_string();
    let git_host = GitHost::resolve(cx, &host)?;
    let host_accounts = accounts(cx, &host);
    if host_accounts.len() < 2 {
        return None;
    }
    let key = repository_key(&host, remote);
    if let Some(detection) = cx
        .try_global::<RepositoryAccessCache>()
        .and_then(|cache| cache.0.get(&key))
        // A detection that learned nothing (e.g. it ran offline) is retried.
        .filter(|detection| !matches!(detection.peek(), Some(None)))
    {
        return Some(detection.clone());
    }

    let remote = ParsedGitRemote {
        owner: remote.owner.clone(),
        repo: remote.repo.clone(),
    };
    let detection = cx
        .spawn(async move |cx| {
            let cx = &*cx;
            let checks = host_accounts.into_iter().map(|account| {
                let host = &host;
                let git_host = &git_host;
                let provider = &provider;
                let remote = &remote;
                let http_client = http_client.clone();
                async move {
                    let (username, secret) =
                        read_credential(cx, &account_credential_url(host, &account.id))
                            .await
                            .log_err()
                            .flatten()?;
                    let access = provider
                        .repository_access(
                            remote,
                            Some(git_host.auth(username, secret)),
                            http_client,
                        )
                        .await
                        .log_err()
                        .flatten()?;
                    Some((account.id, access))
                }
            });
            let access: HashMap<String, RepositoryAccess> = futures::future::join_all(checks)
                .await
                .into_iter()
                .flatten()
                .collect();
            (!access.is_empty()).then_some(access)
        })
        .shared();
    cx.default_global::<RepositoryAccessCache>()
        .0
        .insert(key, detection.clone());
    Some(detection)
}

/// The login of the account `workspace` uses for `host`, for synchronous use
/// during rendering. May briefly lag a connect/disconnect until the async
/// refresh completes.
pub fn connected_username(cx: &App, workspace: Option<EntityId>, host: &str) -> Option<String> {
    active_account(cx, workspace, host).map(|account| account.username)
}

/// Reads the credential of the account `workspace` uses for `host` and
/// converts it into a ready-to-use [`GitHostAuth`]. Returns `None` when the
/// host has no registered provider with an auth flow, or when nothing is
/// connected for it.
pub async fn auth_for_host(
    cx: &AsyncApp,
    workspace: Option<EntityId>,
    host: &str,
) -> Result<Option<GitHostAuth>> {
    let Some(git_host) = GitHost::resolve_async(cx, host) else {
        return Ok(None);
    };
    let Some(account) = cx.update(|cx| active_account(cx, workspace, host)) else {
        return Ok(None);
    };
    Ok(
        read_credential(cx, &account_credential_url(host, &account.id))
            .await?
            .map(|(username, secret)| git_host.auth(username, secret)),
    )
}

/// Reads the credential of the account `workspace` uses for one repository,
/// first detecting which connected account can do the most there when the host
/// has several. Requests scoped to a repository should use this rather than
/// [`auth_for_host`], so each repository gets the account that can use it.
pub async fn auth_for_repository(
    cx: &AsyncApp,
    workspace: Option<EntityId>,
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: &ParsedGitRemote,
    http_client: Arc<dyn HttpClient>,
) -> Result<Option<GitHostAuth>> {
    let Some(host) = provider.base_url().host_str().map(str::to_string) else {
        return Ok(None);
    };
    let Some(git_host) = GitHost::resolve_async(cx, &host) else {
        return Ok(None);
    };
    let detection = cx.update(|cx| detect_repository_access(cx, provider, remote, http_client));
    if let Some(detection) = detection {
        detection.await;
    }
    let Some(account) = cx.update(|cx| account_for_repository(cx, workspace, &host, remote)) else {
        return Ok(None);
    };
    Ok(
        read_credential(cx, &account_credential_url(&host, &account.id))
            .await?
            .map(|(username, secret)| git_host.auth(username, secret)),
    )
}

/// The environment variable a host's command-line tool reads its token from:
/// `gh` for GitHub and GitHub Enterprise, `glab` for GitLab. `None` for
/// Bitbucket, which has no standard tool that takes a token this way.
fn cli_token_variable(git_host: &GitHost) -> Option<&'static str> {
    match git_host.kind() {
        // `gh` sends GH_TOKEN to github.com and GHE.com's `*.ghe.com` tenants,
        // and GH_ENTERPRISE_TOKEN to every Enterprise Server host.
        GitHostAuthKind::GitHub
            if git_host.is_public_instance() || git_host.host().ends_with(".ghe.com") =>
        {
            Some("GH_TOKEN")
        }
        GitHostAuthKind::GitHub => Some("GH_ENTERPRISE_TOKEN"),
        GitHostAuthKind::GitLab => Some("GITLAB_TOKEN"),
        GitHostAuthKind::Bitbucket => None,
    }
}

/// The variables that make `gh` and `glab` act as the account the pull-request
/// panel uses for each repository, so an agent working on a pull request from
/// its own shell does so as the same account. Repositories come in priority
/// order: each variable holds one token, so when two repositories need the same
/// variable the earlier one keeps it.
pub async fn cli_env_for_repositories(
    cx: &AsyncApp,
    workspace: Option<EntityId>,
    repositories: Vec<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)>,
    http_client: Arc<dyn HttpClient>,
) -> HashMap<String, String> {
    let mut env = HashMap::default();
    for (provider, remote) in repositories {
        let Some(host) = provider.base_url().host_str().map(str::to_string) else {
            continue;
        };
        let Some(variable) =
            GitHost::resolve_async(cx, &host).and_then(|git_host| cli_token_variable(&git_host))
        else {
            continue;
        };
        if env.contains_key(variable) {
            continue;
        }
        match auth_for_repository(cx, workspace, provider, &remote, http_client.clone()).await {
            // `gh` and `glab` only take a bare token; a Basic credential has no
            // equivalent variable.
            Ok(Some(GitHostAuth::Bearer(token))) => {
                env.insert(variable.to_string(), token);
            }
            Ok(_) => {}
            Err(error) => {
                log::warn!("git host accounts: no credential for {host}: {error:#}");
            }
        }
    }
    env
}

/// Stores a credential as an account on `host`, makes it the host's default,
/// and returns its id. Reconnecting an account that is already there (same
/// non-empty username) replaces its secret rather than adding a duplicate.
pub async fn add_account(
    cx: &AsyncApp,
    host: &str,
    username: &str,
    secret: &str,
) -> Result<String> {
    let provider = provider(cx)?;
    let mut index = cx.background_spawn(async { load_index() }).await;
    let host_accounts = index.hosts.entry(host.to_string()).or_default();
    let existing = (!username.is_empty())
        .then(|| {
            host_accounts
                .accounts
                .iter()
                .find(|account| account.username == username)
        })
        .flatten()
        .map(|account| account.id.clone());
    let account_id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    provider
        .write_credentials(
            &account_credential_url(host, &account_id),
            username,
            secret.as_bytes(),
            cx,
        )
        .await?;
    if host_accounts.find(&account_id).is_none() {
        host_accounts.accounts.push(GitHostAccount {
            id: account_id.clone(),
            username: username.to_string(),
        });
    }
    host_accounts.default = Some(account_id.clone());
    cx.background_spawn(async move { save_index(&index) })
        .await?;
    cx.update(refresh_connections);
    Ok(account_id)
}

/// Deletes one account's credential and drops it from the host's list.
/// Workspaces that were using it fall back to the host's default.
pub async fn remove_account(cx: &AsyncApp, host: &str, account_id: &str) -> Result<()> {
    let provider = provider(cx)?;
    provider
        .delete_credentials(&account_credential_url(host, account_id), cx)
        .await?;
    let mut index = cx.background_spawn(async { load_index() }).await;
    if let Some(host_accounts) = index.hosts.get_mut(host) {
        host_accounts
            .accounts
            .retain(|account| account.id != account_id);
        if host_accounts.default.as_deref() == Some(account_id) {
            host_accounts.default = host_accounts
                .accounts
                .first()
                .map(|account| account.id.clone());
        }
        if host_accounts.accounts.is_empty() {
            index.hosts.remove(host);
        }
    }
    cx.background_spawn(async move { save_index(&index) })
        .await?;
    cx.update(refresh_connections);
    Ok(())
}

/// Re-reads the account index, adopts any connection made before hosts could
/// hold several accounts, updates the in-memory snapshot, and refreshes open
/// windows so menus reflect the change.
pub fn refresh_connections(cx: &mut App) {
    // Resolved synchronously: the registry lives behind a global that the
    // spawned task cannot borrow across its await points.
    let hosts: Vec<Arc<str>> = connectable_hosts(cx)
        .into_iter()
        .map(|host| host.host)
        .collect();
    cx.spawn(async move |cx| {
        let mut index = cx.background_spawn(async { load_index() }).await;
        let mut adopted = false;
        for host in hosts {
            if index.hosts.contains_key(&*host) {
                continue;
            }
            if let Ok(Some((username, _secret))) =
                read_credential(cx, &host_credential_url(&host)).await
            {
                index.hosts.insert(
                    host.to_string(),
                    HostAccounts {
                        accounts: vec![GitHostAccount {
                            id: LEGACY_ACCOUNT_ID.to_string(),
                            username,
                        }],
                        default: Some(LEGACY_ACCOUNT_ID.to_string()),
                    },
                );
                adopted = true;
            }
        }
        if adopted {
            let snapshot = index.clone();
            cx.background_spawn(async move { save_index(&snapshot) })
                .await
                .log_err();
        }
        cx.update(|cx| {
            cx.set_global(GitHostConnections(index));
            // Which account suits a repository depends on the accounts there are.
            cx.set_global(RepositoryAccessCache::default());
            cx.refresh_windows();
        })
    })
    .detach();
}

/// Registers `on_change` to run whenever the connected accounts change, or a
/// workspace switches which account it uses. Pull-request views use this to
/// reload with the right credentials. The returned [`Subscription`] must be
/// retained for the callback to stay active.
pub fn observe_connections<T: 'static>(
    cx: &mut Context<T>,
    on_change: impl FnMut(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<GitHostConnections>(on_change)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> GitHostAccount {
        GitHostAccount {
            id: id.to_string(),
            username: format!("user-{id}"),
        }
    }

    #[test]
    fn default_account_falls_back_to_first_when_unset_or_stale() {
        let mut host_accounts = HostAccounts {
            accounts: vec![account("a"), account("b")],
            default: Some("b".to_string()),
        };
        assert_eq!(host_accounts.default_account().unwrap().id, "b");
        host_accounts.default = Some("gone".to_string());
        assert_eq!(host_accounts.default_account().unwrap().id, "a");
        host_accounts.default = None;
        assert_eq!(host_accounts.default_account().unwrap().id, "a");
    }

    #[test]
    fn legacy_account_keeps_its_original_keychain_entry() {
        assert_eq!(
            account_credential_url("github.com", LEGACY_ACCOUNT_ID),
            "https://github.com"
        );
        assert_eq!(
            account_credential_url("github.com", "abc"),
            "https://github.com/lathe-git-account/abc"
        );
    }

    #[gpui::test]
    fn workspaces_resolve_their_own_account(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut index = AccountIndex::default();
            index.hosts.insert(
                "github.com".to_string(),
                HostAccounts {
                    accounts: vec![account("work"), account("personal")],
                    default: Some("work".to_string()),
                },
            );
            cx.set_global(GitHostConnections(index));

            let first = EntityId::from(1u64);
            let second = EntityId::from(2u64);
            WorkspaceGitHostAccounts::bind(first, "github.com", Some("personal".into()), cx);

            let active = |workspace, cx: &App| {
                active_account(cx, Some(workspace), "github.com").map(|account| account.id)
            };
            assert_eq!(active(first, cx).as_deref(), Some("personal"));
            assert_eq!(active(second, cx).as_deref(), Some("work"));

            // A pick whose account was removed falls back to the default.
            WorkspaceGitHostAccounts::bind(second, "github.com", Some("deleted".into()), cx);
            assert_eq!(active(second, cx).as_deref(), Some("work"));
            assert!(active_account(cx, Some(first), "gitlab.com").is_none());
        });
    }

    #[gpui::test]
    fn cli_token_variable_matches_what_each_tool_reads(cx: &mut gpui::TestAppContext) {
        let host = |kind, host: &str| GitHost {
            kind,
            host: host.into(),
            display_name: host.into(),
        };
        cx.update(|_| {
            let variable = |git_host: GitHost| cli_token_variable(&git_host);
            assert_eq!(
                variable(host(GitHostAuthKind::GitHub, "github.com")),
                Some("GH_TOKEN")
            );
            assert_eq!(
                variable(host(GitHostAuthKind::GitHub, "acme.ghe.com")),
                Some("GH_TOKEN")
            );
            assert_eq!(
                variable(host(GitHostAuthKind::GitHub, "git.corp.example")),
                Some("GH_ENTERPRISE_TOKEN")
            );
            assert_eq!(
                variable(host(GitHostAuthKind::GitLab, "gitlab.example")),
                Some("GITLAB_TOKEN")
            );
            assert_eq!(
                variable(host(GitHostAuthKind::Bitbucket, "bitbucket.org")),
                None
            );
        });
    }

    struct FakeProvider {
        kind: GitHostAuthKind,
        host: &'static str,
        /// What each token can do, by `owner/repo`.
        access: HashMap<(&'static str, &'static str), RepositoryAccess>,
    }

    #[async_trait::async_trait]
    impl GitHostingProvider for FakeProvider {
        fn name(&self) -> String {
            self.host.to_string()
        }

        fn base_url(&self) -> url::Url {
            url::Url::parse(&format!("https://{}", self.host)).unwrap()
        }

        fn build_commit_permalink(
            &self,
            _remote: &ParsedGitRemote,
            _params: crate::BuildCommitPermalinkParams,
        ) -> url::Url {
            self.base_url()
        }

        fn build_permalink(
            &self,
            _remote: ParsedGitRemote,
            _params: crate::BuildPermalinkParams,
        ) -> url::Url {
            self.base_url()
        }

        fn supports_avatars(&self) -> bool {
            false
        }

        fn format_line_number(&self, line: u32) -> String {
            line.to_string()
        }

        fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
            format!("{start_line}-{end_line}")
        }

        fn parse_remote_url(&self, _url: &str) -> Option<ParsedGitRemote> {
            None
        }

        fn auth_kind(&self) -> Option<GitHostAuthKind> {
            Some(self.kind)
        }

        async fn repository_access(
            &self,
            remote: &ParsedGitRemote,
            auth: Option<GitHostAuth>,
            _http_client: Arc<dyn HttpClient>,
        ) -> Result<Option<RepositoryAccess>> {
            let Some(GitHostAuth::Bearer(token)) = auth else {
                return Ok(Some(RepositoryAccess::None));
            };
            let repository = format!("{}/{}", remote.owner, remote.repo);
            Ok(Some(
                self.access
                    .iter()
                    .find(|((access_token, access_repository), _)| {
                        *access_token == token && *access_repository == repository
                    })
                    .map_or(RepositoryAccess::None, |(_, access)| *access),
            ))
        }
    }

    /// Serves `(username, token)` for each account's keychain url.
    struct FakeCredentials(HashMap<String, (String, String)>);

    impl CredentialsProvider for FakeCredentials {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>>
        {
            let credential = self
                .0
                .get(url)
                .map(|(username, token)| (username.clone(), token.clone().into_bytes()));
            Box::pin(async move { Ok(credential) })
        }

        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[gpui::test]
    async fn agent_env_uses_each_repositorys_account(cx: &mut gpui::TestAppContext) {
        let github: Arc<dyn GitHostingProvider + Send + Sync> = Arc::new(FakeProvider {
            kind: GitHostAuthKind::GitHub,
            host: "github.com",
            access: HashMap::from_iter([
                (("work-token", "acme/private"), RepositoryAccess::Write),
                (("personal-token", "me/dotfiles"), RepositoryAccess::Write),
            ]),
        });
        let enterprise: Arc<dyn GitHostingProvider + Send + Sync> = Arc::new(FakeProvider {
            kind: GitHostAuthKind::GitHub,
            host: "git.corp.example",
            access: HashMap::default(),
        });
        let bitbucket: Arc<dyn GitHostingProvider + Send + Sync> = Arc::new(FakeProvider {
            kind: GitHostAuthKind::Bitbucket,
            host: "bitbucket.org",
            access: HashMap::default(),
        });

        cx.update(|cx| {
            let registry = GitHostingProviderRegistry::new();
            for provider in [&github, &enterprise, &bitbucket] {
                registry.register_hosting_provider(provider.clone());
            }
            GitHostingProviderRegistry::set_global(Arc::new(registry), cx);

            let credential = |host: &str, id: &str, token: &str| {
                (
                    account_credential_url(host, id),
                    (format!("user-{id}"), token.to_string()),
                )
            };
            cx.set_global(GlobalGitHostCredentials(Arc::new(FakeCredentials(
                HashMap::from_iter([
                    credential("github.com", "work", "work-token"),
                    credential("github.com", "personal", "personal-token"),
                    credential("git.corp.example", "corp", "corp-token"),
                    credential("bitbucket.org", "bb", "bb-secret"),
                ]),
            ))));

            let mut index = AccountIndex::default();
            index.hosts.insert(
                "github.com".to_string(),
                HostAccounts {
                    accounts: vec![account("work"), account("personal")],
                    default: Some("personal".to_string()),
                },
            );
            for (host, id) in [("git.corp.example", "corp"), ("bitbucket.org", "bb")] {
                index.hosts.insert(
                    host.to_string(),
                    HostAccounts {
                        accounts: vec![account(id)],
                        default: Some(id.to_string()),
                    },
                );
            }
            cx.set_global(GitHostConnections(index));
        });

        let remote = |owner: &str, repo: &str| ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        };
        let repositories = vec![
            // The active repository: only the work account can see it, so it
            // decides GH_TOKEN even though personal is the host default.
            (github.clone(), remote("acme", "private")),
            (github.clone(), remote("me", "dotfiles")),
            (enterprise.clone(), remote("platform", "service")),
            (bitbucket.clone(), remote("team", "app")),
        ];
        let env = cx
            .update(|cx| {
                cx.spawn(async move |cx| {
                    cli_env_for_repositories(
                        cx,
                        None,
                        repositories,
                        Arc::new(http_client::BlockedHttpClient::new()),
                    )
                    .await
                })
            })
            .await;

        assert_eq!(
            env,
            HashMap::from_iter([
                ("GH_TOKEN".to_string(), "work-token".to_string()),
                ("GH_ENTERPRISE_TOKEN".to_string(), "corp-token".to_string()),
            ])
        );
    }

    #[test]
    fn repository_uses_the_most_capable_account() {
        let host_accounts = HostAccounts {
            accounts: vec![account("work"), account("personal"), account("other")],
            default: Some("personal".to_string()),
        };
        let access = |pairs: &[(&str, RepositoryAccess)]| -> HashMap<String, RepositoryAccess> {
            pairs
                .iter()
                .map(|(id, access)| (id.to_string(), *access))
                .collect()
        };
        let pick = |access: &HashMap<String, RepositoryAccess>, workspace_pick: Option<&str>| {
            host_accounts
                .most_capable_account(access, workspace_pick)
                .map(|account| account.id.clone())
        };

        // A private org repository only the work account can see wins over
        // both the host default and the workspace's own pick.
        let org_repository = access(&[
            ("work", RepositoryAccess::Write),
            ("personal", RepositoryAccess::None),
            ("other", RepositoryAccess::None),
        ]);
        assert_eq!(pick(&org_repository, None).as_deref(), Some("work"));
        assert_eq!(
            pick(&org_repository, Some("personal")).as_deref(),
            Some("work")
        );

        // Pushing beats only reading.
        let fork = access(&[
            ("work", RepositoryAccess::Read),
            ("personal", RepositoryAccess::Write),
        ]);
        assert_eq!(pick(&fork, Some("work")).as_deref(), Some("personal"));

        // Equal access defers to the workspace pick, then the host default.
        let public = access(&[
            ("work", RepositoryAccess::Read),
            ("personal", RepositoryAccess::Read),
            ("other", RepositoryAccess::Read),
        ]);
        assert_eq!(pick(&public, Some("other")).as_deref(), Some("other"));
        assert_eq!(pick(&public, None).as_deref(), Some("personal"));

        // Nothing usable, or nothing known, leaves the choice to the workspace.
        let unreachable = access(&[("work", RepositoryAccess::None)]);
        assert_eq!(pick(&unreachable, None), None);
        assert_eq!(pick(&HashMap::default(), None), None);
    }
}

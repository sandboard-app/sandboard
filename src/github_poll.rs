//! Webhook polling fallback — same Board effects as `POST /api/webhooks/github`.
//!
//! When Settings → Forge enables polling, a background loop mints a GitHub App
//! installation token and scans Review/NeedsHuman PRs plus default-branch tips
//! for repos on live cards. Merges call `complete_for_merged_pr_by(..., "github-poll")`;
//! tip changes call `notify_main_advanced` then Review mergeable catch-up.
//! Newly submitted PR reviews (`CHANGES_REQUESTED` / `COMMENT`) call
//! `apply_pr_review_feedback_by(..., "github-poll")` — pointer steer + Backlog,
//! sharing Board effects with the webhook path. Webhooks keep working in parallel.

use crate::github_app;
use crate::model::{State, MIN_WEBHOOK_POLL_INTERVAL_SECS};
use crate::store::{parse_github_pr_url, Board, SharedBoard};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

/// Actor string written into transition history for poll-driven Done / steer.
pub const POLL_BY: &str = "github-poll";

/// How often the loop re-checks Settings when polling is off.
const DISABLED_RECHECK_SECS: u64 = 60;

/// One warned failure class at a time (config / auth / network).
static LAST_WARN_CLASS: Mutex<Option<&'static str>> = Mutex::new(None);

fn warn_once(class: &'static str, msg: impl std::fmt::Display) {
    let mut g = LAST_WARN_CLASS.lock().unwrap_or_else(|e| e.into_inner());
    if *g == Some(class) {
        return;
    }
    *g = Some(class);
    tracing::warn!(class, "{msg}");
}

fn clear_warn(class: &'static str) {
    let mut g = LAST_WARN_CLASS.lock().unwrap_or_else(|e| e.into_inner());
    if *g == Some(class) {
        *g = None;
    }
}

/// Background loop: sleep → re-read config → tick when enabled.
pub async fn poll_loop(board: SharedBoard) {
    loop {
        let cfg = board.webhook_poll_config();
        let sleep_secs = if cfg.enabled {
            cfg.interval_secs.max(MIN_WEBHOOK_POLL_INTERVAL_SECS)
        } else {
            DISABLED_RECHECK_SECS
        };
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        if !board.webhook_poll_config().enabled {
            continue;
        }
        match tick(&board).await {
            Ok(()) => {
                clear_warn("config");
                clear_warn("auth");
                clear_warn("tick");
            }
            Err(e) => warn_once("tick", format!("webhook poll tick failed: {e}")),
        }
    }
}

/// One poll pass. No-op when disabled or App tokens unavailable.
pub async fn tick(board: &SharedBoard) -> Result<(), String> {
    let cfg = board.webhook_poll_config();
    if !cfg.enabled {
        return Ok(());
    }

    let token = match github_app::host_poll_token(board, cfg.provider_name.as_deref()).await {
        Ok(Some((provider, t))) => {
            clear_warn("config");
            clear_warn("auth");
            tracing::debug!(%provider, "webhook poll using host token from provider");
            t
        }
        Ok(None) => {
            warn_once(
                "config",
                "webhook poll enabled but Forge credential provider is unset or not ready; skipping",
            );
            return Ok(());
        }
        Err(e) => {
            warn_once("auth", format!("webhook poll token failed: {e}"));
            return Err(e.to_string());
        }
    };

    let targets = collect_targets(board);
    if targets.prs.is_empty() && targets.repos.is_empty() {
        return Ok(());
    }

    for pr in &targets.prs {
        match fetch_pull(&token, &pr.owner_repo, pr.number).await {
            Ok(Some(info)) if info.merged => {
                let url = info
                    .html_url
                    .unwrap_or_else(|| format!("https://github.com/{}/pull/{}", pr.owner_repo, pr.number));
                if let Some(id) =
                    board.complete_for_merged_pr_by(&url, Some(pr.number), POLL_BY)
                {
                    tracing::info!(id, pr = %pr.owner_repo, number = pr.number, "poll: PR merged → Done");
                    let _ = crate::supervisor::process_sibling_review_catch_up(board, id).await;
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(
                    repo = %pr.owner_repo,
                    number = pr.number,
                    error = %e,
                    "poll: PR fetch failed"
                );
            }
        }

        match fetch_reviews(&token, &pr.owner_repo, pr.number).await {
            Ok(reviews) => apply_new_reviews(board, pr, &reviews),
            Err(e) => {
                tracing::debug!(
                    repo = %pr.owner_repo,
                    number = pr.number,
                    error = %e,
                    "poll: PR reviews fetch failed"
                );
            }
        }
    }

    for repo in &targets.repos {
        match fetch_default_tip(&token, repo).await {
            Ok(Some((branch, sha))) => {
                let prev = board.webhook_poll_tip(repo);
                board.set_webhook_poll_tip(repo, &sha);
                if prev.as_deref() == Some(sha.as_str()) {
                    continue;
                }
                // First observation only seeds the tip — avoid MainAdvanced storms
                // on enable. Subsequent changes match webhook push semantics.
                if prev.is_none() {
                    tracing::debug!(%repo, %sha, "poll: seeded default-branch tip");
                    continue;
                }
                let ref_name = format!("refs/heads/{branch}");
                let _ = board.notify_main_advanced(repo, &ref_name, Some(sha.clone()));
                let _ =
                    crate::supervisor::process_main_advanced_review_catch_up(board, repo).await;
                tracing::info!(%repo, %branch, %sha, "poll: default branch advanced");
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(%repo, error = %e, "poll: tip fetch failed");
            }
        }
    }

    Ok(())
}

/// Apply newly submitted actionable reviews; seed cursor on first observation.
fn apply_new_reviews(board: &SharedBoard, pr: &PrTarget, reviews: &[ReviewInfo]) {
    let max_id = reviews.iter().map(|r| r.id).max().unwrap_or(0);
    let prev = board.webhook_poll_pr_review_cursor(&pr.owner_repo, pr.number);

    // Always advance the cursor to the tip we observed (including empty → 0)
    // so first-observation seed does not re-fire forever.
    board.set_webhook_poll_pr_review_cursor(&pr.owner_repo, pr.number, max_id);

    let Some(cursor) = prev else {
        tracing::debug!(
            repo = %pr.owner_repo,
            number = pr.number,
            max_id,
            "poll: seeded PR review cursor"
        );
        return;
    };

    let html_url = format!("https://github.com/{}/pull/{}", pr.owner_repo, pr.number);
    let mut new_reviews: Vec<&ReviewInfo> = reviews.iter().filter(|r| r.id > cursor).collect();
    new_reviews.sort_by_key(|r| r.id);

    for review in new_reviews {
        if !Board::is_actionable_pr_review_state(&review.state) {
            continue;
        }
        if let Some(id) =
            board.apply_pr_review_feedback_by(&html_url, Some(pr.number), &review.state, POLL_BY)
        {
            tracing::info!(
                id,
                pr = %pr.owner_repo,
                number = pr.number,
                review_id = review.id,
                state = %review.state,
                "poll: PR review feedback → Backlog"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PrTarget {
    owner_repo: String,
    number: u64,
}

struct Targets {
    prs: Vec<PrTarget>,
    repos: BTreeSet<String>,
}

fn collect_targets(board: &SharedBoard) -> Targets {
    let mut prs = BTreeSet::new();
    let mut repos = BTreeSet::new();

    let items = board.snapshot().items;

    for item in items {
        match item.state {
            State::Review | State::NeedsHuman => {
                for url in item.pr_urls() {
                    if let Some((owner_repo, number)) = parse_github_pr_url(url) {
                        repos.insert(owner_repo.clone());
                        prs.insert(PrTarget { owner_repo, number });
                    }
                }
            }
            State::Claimed | State::Running => {
                for url in item.pr_urls() {
                    if let Some((owner_repo, number)) = parse_github_pr_url(url) {
                        // Live cards with a PR: poll reviews (merge complete still
                        // only Done when every listed PR is merged).
                        prs.insert(PrTarget {
                            owner_repo: owner_repo.clone(),
                            number,
                        });
                        repos.insert(owner_repo);
                    }
                }
                if let Ok(Some(repo)) = board.resolve_card_repo(item.id) {
                    let upstream = repo.upstream.trim();
                    if !upstream.is_empty() {
                        repos.insert(upstream.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    Targets {
        prs: prs.into_iter().collect(),
        repos,
    }
}

#[derive(Debug)]
struct PullInfo {
    merged: bool,
    html_url: Option<String>,
}

#[derive(Debug, Clone)]
struct ReviewInfo {
    id: u64,
    state: String,
}

async fn fetch_pull(
    token: &str,
    owner_repo: &str,
    number: u64,
) -> Result<Option<PullInfo>, String> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        merged: bool,
        html_url: Option<String>,
    }
    let url = format!(
        "{}/repos/{owner_repo}/pulls/{number}",
        github_app::github_api_base()
    );
    let resp = client()?
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET pull: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GET pull HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let body: Resp = resp
        .json()
        .await
        .map_err(|e| format!("GET pull json: {e}"))?;
    Ok(Some(PullInfo {
        merged: body.merged,
        html_url: body.html_url,
    }))
}

async fn fetch_reviews(
    token: &str,
    owner_repo: &str,
    number: u64,
) -> Result<Vec<ReviewInfo>, String> {
    #[derive(Deserialize)]
    struct Resp {
        id: u64,
        #[serde(default)]
        state: String,
    }
    let url = format!(
        "{}/repos/{owner_repo}/pulls/{number}/reviews",
        github_app::github_api_base()
    );
    let resp = client()?
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET reviews: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GET reviews HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let body: Vec<Resp> = resp
        .json()
        .await
        .map_err(|e| format!("GET reviews json: {e}"))?;
    Ok(body
        .into_iter()
        .map(|r| ReviewInfo {
            id: r.id,
            state: r.state,
        })
        .collect())
}

async fn fetch_default_tip(token: &str, owner_repo: &str) -> Result<Option<(String, String)>, String> {
    #[derive(Deserialize)]
    struct RepoResp {
        default_branch: Option<String>,
    }
    #[derive(Deserialize)]
    struct RefObject {
        sha: String,
    }
    #[derive(Deserialize)]
    struct RefResp {
        object: RefObject,
    }

    let repo_url = format!("{}/repos/{owner_repo}", github_app::github_api_base());
    let resp = client()?
        .get(&repo_url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET repo: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GET repo HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let repo: RepoResp = resp
        .json()
        .await
        .map_err(|e| format!("GET repo json: {e}"))?;
    let branch = repo
        .default_branch
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".into());

    let ref_url = format!(
        "{}/repos/{owner_repo}/git/ref/heads/{branch}",
        github_app::github_api_base()
    );
    let resp = client()?
        .get(&ref_url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET ref: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GET ref HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let r: RefResp = resp
        .json()
        .await
        .map_err(|e| format!("GET ref json: {e}"))?;
    let sha = r.object.sha.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    Ok(Some((branch, sha)))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("sandboard")
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Origin, WebhookPollConfig};
    use crate::store::Board;
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex, MutexGuard};

    mod github_api_env {
        use super::*;
        static LOCK: Mutex<()> = Mutex::new(());

        pub struct Guard {
            prev: Option<String>,
            _lock: MutexGuard<'static, ()>,
        }

        impl Guard {
            pub fn set(base: &str) -> Self {
                let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let prev = std::env::var("SANDBOARD_GITHUB_API").ok();
                std::env::set_var("SANDBOARD_GITHUB_API", base);
                Self { prev, _lock }
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var("SANDBOARD_GITHUB_API", v),
                    None => std::env::remove_var("SANDBOARD_GITHUB_API"),
                }
            }
        }
    }

    fn test_rsa_pem() -> String {
        include_str!("testdata/github_app_test_rsa.pem").to_string()
    }

    fn test_board(tag: &str) -> (std::path::PathBuf, SharedBoard, crate::secrets::master_key_env::Guard) {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-ghpoll-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);
        let board: SharedBoard = Arc::new(Board::new(
            crate::schema::Schema::default(),
            dir.join("board.json"),
        ));
        (dir, board, env)
    }

    fn seal_test_app(board: &SharedBoard) {
        board
            .set_github_app_bundle(&crate::secrets::GitHubAppBundle {
                app_id: "123456".into(),
                private_key_pem: test_rsa_pem(),
                ..Default::default()
            })
            .expect("seal onto provider");
        board.set_github_app_installation_id(Some(99));
    }

    async fn spawn_poll_mock(
        merged: bool,
        tip_sha: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        spawn_poll_mock_for_pr(merged, tip_sha, None, Arc::new(Mutex::new(Vec::new()))).await
    }

    /// When `merged_only_number` is set, only that PR number reports `merged: true`.
    /// `reviews` is shared state the mock returns for GET .../pulls/{n}/reviews.
    async fn spawn_poll_mock_for_pr(
        merged: bool,
        tip_sha: &'static str,
        merged_only_number: Option<u64>,
        reviews: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::Path;

        let reviews_get = reviews.clone();
        let app = Router::new()
            .route(
                "/app/installations/{id}/access_tokens",
                axum::routing::post(|| async {
                    Json(serde_json::json!({
                        "token": "ghs_poll_token",
                        "expires_at": "2099-01-01T00:00:00Z"
                    }))
                }),
            )
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/reviews",
                get(move || {
                    let reviews_get = reviews_get.clone();
                    async move {
                        let list = reviews_get.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        Json(serde_json::Value::Array(list))
                    }
                }),
            )
            .route(
                "/repos/{owner}/{repo}/pulls/{number}",
                get(move |Path((_owner, _repo, number)): Path<(String, String, u64)>| async move {
                    let is_merged = match merged_only_number {
                        Some(n) => merged && number == n,
                        None => merged,
                    };
                    Json(serde_json::json!({
                        "merged": is_merged,
                        "html_url": format!("https://github.com/acme/widgets/pull/{number}")
                    }))
                }),
            )
            .route(
                "/repos/{owner}/{repo}",
                get(|| async {
                    Json(serde_json::json!({ "default_branch": "main" }))
                }),
            )
            .route(
                "/repos/{owner}/{repo}/git/ref/heads/{branch}",
                get(move || async move {
                    Json(serde_json::json!({
                        "object": { "sha": tip_sha, "type": "commit" }
                    }))
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });
        (format!("http://{addr}"), handle)
    }

    fn review_card(board: &SharedBoard) -> crate::model::ItemId {
        let p = board
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = board
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(t.id, State::Shaping, "test", None);
        let _ = board.transition(t.id, State::Backlog, "test", None);
        let _ = board.transition(t.id, State::Claimed, "test", None);
        let _ = board.transition(t.id, State::Running, "test", None);
        let _ = board.transition(t.id, State::Review, "test", None);
        board.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/widgets/pull/7".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/widgets", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/widgets", "sandboard/t")),
                ..Default::default()
            }),
        );
        t.id
    }

    #[test]
    fn normalize_clamps_interval() {
        let cfg = WebhookPollConfig {
            enabled: true,
            interval_secs: 5,
            provider_name: None,
        }
        .normalized();
        assert_eq!(cfg.interval_secs, MIN_WEBHOOK_POLL_INTERVAL_SECS);
        assert!(cfg.enabled);
    }

    #[tokio::test]
    async fn tick_disabled_is_noop() {
        let (dir, board, _env) = test_board("disabled");
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: false,
            interval_secs: 60,
            provider_name: None,
        });
        // Dead API — must not be contacted.
        let _api = github_api_env::Guard::set("http://127.0.0.1:1");
        tick(&board).await.expect("disabled tick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tick_merged_pr_completes_review_card() {
        let (dir, board, _env) = test_board("merge");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });

        let p = board
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = board
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let sibling = board
            .create(
                Some(p.id),
                "Sibling Review",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        for id in [t.id, sibling.id] {
            let _ = board.transition(id, State::Shaping, "test", None);
            let _ = board.transition(id, State::Backlog, "test", None);
            let _ = board.transition(id, State::Claimed, "test", None);
            let _ = board.transition(id, State::Running, "test", None);
            let _ = board.transition(id, State::Review, "test", None);
        }
        board.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/widgets/pull/7".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/widgets", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/widgets", "sandboard/t")),
                ..Default::default()
            }),
        );
        board.set_pull_request(
            sibling.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/widgets/pull/8".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/widgets", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/widgets", "sandboard/sib")),
                ..Default::default()
            }),
        );

        let (base, handle) =
            spawn_poll_mock_for_pr(true, "aaa111", Some(7), Arc::new(Mutex::new(Vec::new()))).await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("tick");
        assert_eq!(board.get(t.id).unwrap().state, State::Done);
        let by = board
            .get(t.id)
            .unwrap()
            .history
            .last()
            .map(|h| h.by.clone())
            .unwrap_or_default();
        assert_eq!(by, POLL_BY);

        // Poll merge completes via the same Board path as webhooks; sibling
        // Review catch-up observes mergeable (may defer/queue without App mock).
        let sib = board.get(sibling.id).unwrap();
        assert_eq!(sib.state, State::Review);
        assert!(
            board
                .identify_behind_sibling_prs(t.id)
                .iter()
                .any(|i| i.id == sibling.id)
                || sib.rebase_requested,
            "github-poll merge must leave sibling as a catch-up target like webhook"
        );

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tip_change_notifies_main_advanced_once_after_seed() {
        let (dir, board, _env) = test_board("tip");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });

        let p = board
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = board
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(t.id, State::Shaping, "test", None);
        let _ = board.transition(t.id, State::Backlog, "test", None);
        let _ = board.transition(t.id, State::Claimed, "test", None);
        let _ = board.transition(t.id, State::Running, "test", None);
        let _ = board.transition(t.id, State::Review, "test", None);
        board.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/widgets/pull/7".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/widgets", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/widgets", "sandboard/t")),
                ..Default::default()
            }),
        );

        // Seed tip without MainAdvanced.
        board.set_webhook_poll_tip("acme/widgets", "sha_old");

        let mut events = board.subscribe();
        let (base, handle) = spawn_poll_mock(false, "sha_new").await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("tick");
        assert_eq!(
            board.webhook_poll_tip("acme/widgets").as_deref(),
            Some("sha_new")
        );

        let mut saw_main = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, crate::events::BoardEvent::MainAdvanced { .. }) {
                saw_main = true;
                break;
            }
        }
        assert!(saw_main, "expected MainAdvanced after tip change");

        // Second tick with same tip must not fire again.
        let mut events2 = board.subscribe();
        tick(&board).await.expect("tick2");
        let mut again = false;
        while let Ok(ev) = events2.try_recv() {
            if matches!(ev, crate::events::BoardEvent::MainAdvanced { .. }) {
                again = true;
                break;
            }
        }
        assert!(!again, "idempotent tip must not re-fire MainAdvanced");

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn review_seed_only_first_tick_does_not_bounce() {
        let (dir, board, _env) = test_board("review-seed");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });
        let id = review_card(&board);

        let reviews = Arc::new(Mutex::new(vec![serde_json::json!({
            "id": 1001,
            "state": "CHANGES_REQUESTED",
            "body": "please fix the flaky test"
        })]));
        let (base, handle) = spawn_poll_mock_for_pr(false, "sha", None, reviews).await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("seed tick");
        assert_eq!(
            board.get(id).unwrap().state,
            State::Review,
            "first observation must only seed the cursor"
        );
        assert_eq!(
            board.webhook_poll_pr_review_cursor("acme/widgets", 7),
            Some(1001)
        );

        // Same reviews again — still no bounce.
        tick(&board).await.expect("idempotent tick");
        assert_eq!(board.get(id).unwrap().state, State::Review);

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn new_changes_requested_review_steers_to_backlog() {
        let (dir, board, _env) = test_board("review-cr");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });
        let id = review_card(&board);
        board.set_webhook_poll_pr_review_cursor("acme/widgets", 7, 1000);

        let reviews = Arc::new(Mutex::new(vec![
            serde_json::json!({ "id": 1000, "state": "COMMENTED", "body": "old" }),
            serde_json::json!({
                "id": 1002,
                "state": "CHANGES_REQUESTED",
                "body": "must not appear in steer note"
            }),
        ]));
        let (base, handle) = spawn_poll_mock_for_pr(false, "sha", None, reviews).await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("tick");
        let item = board.get(id).unwrap();
        assert_eq!(item.state, State::Backlog);
        let by = item.history.last().map(|h| h.by.clone()).unwrap_or_default();
        assert_eq!(by, POLL_BY, "poll must use github-poll actor");
        let note = item.notes.last().expect("pointer steer").text.clone();
        assert!(
            note.contains("PR review feedback") && note.contains("gh"),
            "pointer-style note expected, got: {note}"
        );
        assert!(
            !note.contains("must not appear"),
            "must not dump review body: {note}"
        );
        assert_eq!(
            board.webhook_poll_pr_review_cursor("acme/widgets", 7),
            Some(1002)
        );

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn new_comment_review_same_backlog_path() {
        let (dir, board, _env) = test_board("review-comment");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });
        let id = review_card(&board);
        board.set_webhook_poll_pr_review_cursor("acme/widgets", 7, 50);

        let reviews = Arc::new(Mutex::new(vec![serde_json::json!({
            "id": 51,
            "state": "COMMENTED",
            "body": "nit: rename this"
        })]));
        let (base, handle) = spawn_poll_mock_for_pr(false, "sha", None, reviews).await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("tick");
        let item = board.get(id).unwrap();
        assert_eq!(item.state, State::Backlog);
        assert_eq!(
            item.history.last().map(|h| h.by.as_str()),
            Some(POLL_BY)
        );
        let note = item.notes.last().expect("pointer").text.clone();
        assert!(note.contains("PR review feedback") && note.contains("gh"));
        assert!(!note.contains("nit: rename"));

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn approved_review_does_not_mutate_board() {
        let (dir, board, _env) = test_board("review-approved");
        seal_test_app(&board);
        board.set_webhook_poll_config(WebhookPollConfig {
            enabled: true,
            interval_secs: 60,
            provider_name: Some("github-app".into()),
        });
        let id = review_card(&board);
        board.set_webhook_poll_pr_review_cursor("acme/widgets", 7, 1);

        let reviews = Arc::new(Mutex::new(vec![serde_json::json!({
            "id": 2,
            "state": "APPROVED",
            "body": "lgtm"
        })]));
        let (base, handle) = spawn_poll_mock_for_pr(false, "sha", None, reviews).await;
        let _api = github_api_env::Guard::set(&base);

        tick(&board).await.expect("tick");
        assert_eq!(board.get(id).unwrap().state, State::Review);
        assert_eq!(
            board.webhook_poll_pr_review_cursor("acme/widgets", 7),
            Some(2),
            "cursor still advances past APPROVED"
        );

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_github_pr_url_feeds_targets() {
        let (owner, n) = parse_github_pr_url("https://github.com/Acme/Widgets/pull/42").unwrap();
        assert_eq!(owner, "Acme/Widgets");
        assert_eq!(n, 42);
    }
}

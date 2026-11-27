//! Seam: the GitHub App client against a local HTTP server that replays
//! recorded real GitHub API responses (see `common/github.rs`): pagination,
//! the `403` rate limit and the push-access check. No database, so it runs on
//! all 3 OSes. The App JWT is really verified by the replay server.

mod common;

use std::sync::Arc;

use common::github::{self, *};
use traction_server::config::Github;
use traction_server::github::{GithubClient, GithubError};

const NOW: u64 = 1_800_000_000;

fn client(fake: &FakeGithub, key_pem: &str) -> Arc<GithubClient> {
    let config = Github {
        api_base_url: fake.base.clone(),
        app_id: APP_ID.into(),
        per_page: 2,
        ..Github::default()
    };
    Arc::new(GithubClient::new(&config, key_pem).unwrap())
}

async fn run<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, GithubError> + Send + 'static,
) -> Result<T, GithubError> {
    tokio::task::spawn_blocking(work).await.unwrap()
}

#[tokio::test]
async fn push_access_is_read_from_the_users_token() {
    let fake = github::start().await;
    let c = client(&fake, APP_KEY_PEM);

    let c1 = Arc::clone(&c);
    let user = run(move || c1.user(USER_TOKEN, NOW)).await.unwrap();
    assert_eq!((user.login.as_str(), user.id), ("octocat", 583_231));

    let c1 = Arc::clone(&c);
    let access = run(move || c1.repo_access(USER_TOKEN, REPO, NOW))
        .await
        .unwrap();
    assert_eq!(
        (access.id, access.full_name.as_str(), access.push),
        (1_296_269, REPO, true)
    );

    let c1 = Arc::clone(&c);
    let access = run(move || c1.repo_access(READONLY_USER_TOKEN, REPO, NOW))
        .await
        .unwrap();
    assert!(!access.push, "read-only users have no push permission");

    let c1 = Arc::clone(&c);
    assert_eq!(
        run(move || c1.user("ghu_wrong", NOW)).await,
        Err(GithubError::Unauthorized)
    );
    let c1 = Arc::clone(&c);
    assert_eq!(
        run(move || c1.repo_access(USER_TOKEN, "octocat/Nope", NOW)).await,
        Err(GithubError::NotFound)
    );
}

#[tokio::test]
async fn installation_tokens_need_a_valid_rs256_app_jwt() {
    let fake = github::start().await;
    let good = client(&fake, APP_KEY_PEM);
    let c = Arc::clone(&good);
    assert_eq!(
        run(move || c.installation_for_repo(REPO, NOW)).await,
        Ok(INSTALLATION_ID)
    );
    let c = Arc::clone(&good);
    assert_eq!(
        run(move || c.installation_token(INSTALLATION_ID, NOW))
            .await
            .as_deref(),
        Ok(INSTALLATION_TOKEN)
    );

    // An App key GitHub does not know signs a JWT GitHub refuses.
    let other = client(&fake, include_str!("data/idp_other_rsa.pem"));
    assert_eq!(
        run(move || other.installation_for_repo(REPO, NOW)).await,
        Err(GithubError::Unauthorized)
    );

    // The App not installed on the repo.
    *fake.state.installed.lock().unwrap() = false;
    let c = Arc::clone(&good);
    assert_eq!(
        run(move || c.installation_for_repo(REPO, NOW)).await,
        Err(GithubError::NotFound)
    );
}

#[tokio::test]
async fn commits_are_fetched_across_pages_as_oids_and_timestamps_only() {
    let fake = github::start().await;
    let c = client(&fake, APP_KEY_PEM);
    let commits = run(move || c.fetch_commits(INSTALLATION_TOKEN, REPO, None, NOW))
        .await
        .unwrap();
    let oids: Vec<&str> = commits.iter().map(|c| c.oid.as_str()).collect();
    assert_eq!(
        oids,
        [
            "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d",
            "762941318ee16e59dabbacb1b4049eec22f0d303",
            "553c2077f0edc3d5dc5d17262f6aa498e69d6f8e"
        ]
    );
    // 2012-03-06T23:06:50Z, 2011-09-14T04:42:41Z, 2011-01-26T19:06:08Z.
    let committed: Vec<i64> = commits.iter().map(|c| c.committed_at_ms).collect();
    assert_eq!(
        committed,
        [1_331_075_210_000, 1_315_975_361_000, 1_296_068_768_000]
    );
    let logins: Vec<Option<&str>> = commits.iter().map(|c| c.author_login.as_deref()).collect();
    assert_eq!(
        logins,
        [Some("octocat"), Some("Spaceghost"), Some("Cameron423698")]
    );

    let recorded = fake.state.recorded.lock().unwrap();
    assert_eq!(
        recorded.requests,
        [
            "/repos/octocat/Hello-World/commits?per_page=2",
            "/repositories/1296269/commits?page=2&per_page=2"
        ]
    );
    // Commits are fetched with the installation token, not the user's.
    assert!(recorded.tokens.iter().all(|t| t == INSTALLATION_TOKEN));
}

#[tokio::test]
async fn an_incremental_fetch_sends_since() {
    let fake = github::start().await;
    let c = client(&fake, APP_KEY_PEM);
    run(move || c.fetch_commits(INSTALLATION_TOKEN, REPO, Some(1_331_075_210_000), NOW))
        .await
        .unwrap();
    let recorded = fake.state.recorded.lock().unwrap();
    assert_eq!(
        recorded.requests[0],
        "/repos/octocat/Hello-World/commits?per_page=2&since=2012-03-06T23:06:50Z"
    );
}

#[tokio::test]
async fn a_403_rate_limit_is_reported_with_the_wait_and_a_retry_succeeds() {
    let fake = github::start().await;
    *fake.state.rate_limit_commits.lock().unwrap() = 1;
    *fake.state.reset_at.lock().unwrap() = NOW + 120;
    let c = client(&fake, APP_KEY_PEM);
    let c1 = Arc::clone(&c);
    assert_eq!(
        run(move || c1.fetch_commits(INSTALLATION_TOKEN, REPO, None, NOW)).await,
        Err(GithubError::RateLimited {
            retry_after_sec: 120
        })
    );
    // Once the window passes the same fetch completes.
    let commits = run(move || c.fetch_commits(INSTALLATION_TOKEN, REPO, None, NOW + 121))
        .await
        .unwrap();
    assert_eq!(commits.len(), 3);
}

#[tokio::test]
async fn a_rate_limit_on_the_second_page_fails_the_fetch_with_no_partial_result() {
    let fake = github::start().await;
    // Page 1 passes, page 2 is limited.
    *fake.state.pass_first.lock().unwrap() = 1;
    *fake.state.rate_limit_commits.lock().unwrap() = 1;
    *fake.state.reset_at.lock().unwrap() = NOW + 30;
    let c = client(&fake, APP_KEY_PEM);
    let result = run(move || c.fetch_commits(INSTALLATION_TOKEN, REPO, None, NOW)).await;
    assert_eq!(
        result,
        Err(GithubError::RateLimited {
            retry_after_sec: 30
        })
    );
}

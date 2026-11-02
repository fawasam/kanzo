//! The narrow GitLab.com integration: a task links one merge request and Kanzo
//! reads its current state on demand. A token is read only from KANZO_GITLAB_TOKEN,
//! so credentials never enter the board database or its git-synced data folder.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::Agent;

const WEB: &str = "https://gitlab.com/";
const API: &str = "https://gitlab.com/api/v4";

/// A local credential file, deliberately outside the board folder: `kanzo sync`
/// can never add it to the board's repository.
fn token_path() -> std::result::Result<PathBuf, Error> {
    dirs::config_dir()
        .map(|dir| dir.join("kanzo").join("gitlab-token"))
        .ok_or_else(|| Error::Fetch("cannot find a local configuration folder for the GitLab token".into()))
}

fn token() -> std::result::Result<Option<String>, Error> {
    if let Some(token) = env::var("KANZO_GITLAB_TOKEN").ok().filter(|token| !token.trim().is_empty()) {
        return Ok(Some(token));
    }
    let path = token_path()?;
    match fs::read_to_string(&path) {
        Ok(token) => Ok((!token.trim().is_empty()).then(|| token.trim().into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Fetch(format!("cannot read local GitLab token: {error}"))),
    }
}

/// Whether a personal token is available from the environment or local-only
/// configuration. Its value is never returned to the browser.
pub fn is_configured() -> std::result::Result<bool, Error> {
    Ok(token()?.is_some())
}

/// Saves a token outside Kanzo's Git-synced board data. Passing an empty value removes
/// the local token; an environment token, when present, still takes precedence.
pub fn set_token(value: &str) -> std::result::Result<bool, Error> {
    let path = token_path()?;
    let value = value.trim();
    if value.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(Error::Fetch(format!("cannot clear local GitLab token: {e}"))),
        }
    }
    let parent = path.parent().expect("token path has a parent");
    fs::create_dir_all(parent).map_err(|e| Error::Fetch(format!("cannot create local GitLab configuration: {e}")))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| Error::Fetch(format!("cannot save local GitLab token: {e}")))?;
    file.write_all(value.as_bytes()).map_err(|e| Error::Fetch(format!("cannot save local GitLab token: {e}")))?;
    file.write_all(b"\n").map_err(|e| Error::Fetch(format!("cannot save local GitLab token: {e}")))?;
    Ok(true)
}

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Fetch(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Fetch(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// The safe, shareable link stored on a task. This first release accepts GitLab.com
/// only: arbitrary GitLab hosts would turn a pasted card link into a server-side
/// request, and need a dedicated self-hosted-instance trust setting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRequestLink {
    project: String,
    iid: u64,
    url: String,
}

fn parse_merge_request_url(value: &str) -> std::result::Result<MergeRequestLink, Error> {
    let value = value.trim();
    let tail = value
        .strip_prefix(WEB)
        .ok_or_else(|| Error::Invalid("GitLab link must be an https://gitlab.com merge request URL".into()))?;
    let (project, iid) = tail.split_once("/-/merge_requests/").ok_or_else(|| {
        Error::Invalid(
            "GitLab link must point to a merge request, like https://gitlab.com/group/project/-/merge_requests/12"
                .into(),
        )
    })?;
    if project.is_empty() || project.split('/').any(|part| part.is_empty() || !valid_project_part(part)) {
        return Err(Error::Invalid("GitLab project path is invalid".into()));
    }
    let iid = iid
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .parse::<u64>()
        .map_err(|_| Error::Invalid("GitLab merge request number is invalid".into()))?;
    if iid == 0 {
        return Err(Error::Invalid("GitLab merge request number is invalid".into()));
    }
    Ok(MergeRequestLink { project: project.into(), iid, url: format!("{WEB}{project}/-/merge_requests/{iid}") })
}

fn valid_project_part(part: &str) -> bool {
    part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Validates and canonicalizes a value stored on a task. A blank field removes the
/// link rather than leaving an invalid empty string in a board synced between machines.
pub fn normalize_merge_request_url(value: &str) -> std::result::Result<Option<String>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_merge_request_url(value).map(|link| Some(link.url)).map_err(|e| e.to_string())
}

/// Current merge-request and latest-pipeline state, sent to the browser on refresh.
#[derive(Debug, Serialize)]
pub struct MergeRequestStatus {
    pub url: String,
    pub title: String,
    pub state: String,
    pub draft: bool,
    pub merge_status: Option<String>,
    pub pipeline_status: Option<String>,
    pub pipeline_url: Option<String>,
}

#[derive(Deserialize)]
struct ApiMergeRequest {
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    detailed_merge_status: Option<String>,
    head_pipeline: Option<ApiPipeline>,
}

#[derive(Deserialize)]
struct ApiPipeline {
    status: String,
    web_url: Option<String>,
}

/// Reads one merge request from the public GitLab API. `KANZO_GITLAB_TOKEN` is
/// optional for public projects and is sent only to gitlab.com when present.
pub fn merge_request(value: &str) -> std::result::Result<MergeRequestStatus, Error> {
    let link = parse_merge_request_url(value)?;
    let project = percent_encode(&link.project);
    let api = format!("{API}/projects/{project}/merge_requests/{}", link.iid);
    let agent: Agent = Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut request = agent.get(&api).header("Accept", "application/json");
    if let Some(token) = token()? {
        request = request.header("PRIVATE-TOKEN", &token);
    }
    let mut response =
        request.call().map_err(|e| Error::Fetch(format!("GitLab could not load this merge request: {e}")))?;
    let mr: ApiMergeRequest = response
        .body_mut()
        .read_json()
        .map_err(|e| Error::Fetch(format!("GitLab returned an unreadable merge request: {e}")))?;
    let pipeline = mr.head_pipeline;
    Ok(MergeRequestStatus {
        url: link.url,
        title: mr.title,
        state: mr.state,
        draft: mr.draft,
        merge_status: mr.detailed_merge_status,
        pipeline_status: pipeline.as_ref().map(|p| p.status.clone()),
        pipeline_url: pipeline.and_then(|p| p.web_url),
    })
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_a_merge_request_url() {
        assert_eq!(
            normalize_merge_request_url(" https://gitlab.com/team/platform/-/merge_requests/42?tab=diffs ").unwrap(),
            Some("https://gitlab.com/team/platform/-/merge_requests/42".into())
        );
    }

    #[test]
    fn rejects_non_gitlab_and_non_merge_request_links() {
        assert!(normalize_merge_request_url("https://example.com/team/project/-/merge_requests/42").is_err());
        assert!(normalize_merge_request_url("https://gitlab.com/team/project/-/issues/42").is_err());
    }

    #[test]
    fn blank_link_clears_the_field() {
        assert_eq!(normalize_merge_request_url("  ").unwrap(), None);
    }
}

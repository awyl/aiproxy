use crate::skills::{Bundle, MAX_BUNDLE, valid_name, validate_files};
use reqwest::{Url, redirect::Policy};
use std::{
    fs,
    io::{self, Write},
    path::Path,
    time::Duration,
};

/// Fetch one live revision and write it to a new directory. Nothing is executed.
pub async fn fetch(
    mut url: Url,
    token: &str,
    name: &str,
    revision: u64,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !valid_name(name)
        || token.trim().is_empty()
    {
        return Err("expected a skill name, token, and HTTPS or loopback HTTP base URL".into());
    }
    url.path_segments_mut()
        .map_err(|_| "invalid base URL")?
        .pop_if_empty()
        .extend(["api", "skills", name, "bundle"]);
    url.query_pairs_mut()
        .append_pair("revision", &revision.to_string());
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.without_url())?;
    if !response.status().is_success() {
        return Err(format!(
            "skill bundle request returned HTTP {}",
            response.status().as_u16()
        )
        .into());
    }
    let bytes = crate::skills_github::bounded_body(response, MAX_BUNDLE * 2)
        .await
        .ok_or("incomplete or oversized bundle")?;
    let bundle: Bundle = serde_json::from_slice(&bytes)?;
    if bundle.revision != revision || !bundle.files.contains_key("SKILL.md") {
        return Err("wrong revision or missing SKILL.md".into());
    }
    validate_files(&bundle.files)?;
    let files = bundle
        .files
        .into_iter()
        .map(|(path, file)| Ok((path, file.bytes()?, file.executable)))
        .collect::<Result<Vec<_>, crate::skills::SkillsError>>()?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let leaf = output.file_name().ok_or("expected a new directory name")?;
    let root = parent.canonicalize()?.join(leaf);
    let mut dir = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    dir.create(&root)?;
    let result: io::Result<()> = (|| {
        for (path, bytes, executable) in files {
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            file.write_all(&bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(if executable {
                    0o700
                } else {
                    0o600
                }))?;
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = fs::remove_dir_all(&root) {
            return Err(format!(
                "could not write bundle: {error}; could not clean up {}: {cleanup}",
                root.display()
            )
            .into());
        }
        return Err(error.into());
    }
    Ok(())
}

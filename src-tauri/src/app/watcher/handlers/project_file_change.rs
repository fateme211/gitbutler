use std::vec;

use anyhow::{Context, Result};

use crate::{
    app::{
        gb_repository, project_repository,
        reader::{self, Reader},
    },
    deltas, projects,
};

use super::events;

pub struct Handler<'listener> {
    project_id: String,
    project_store: projects::Storage,
    gb_repository: &'listener gb_repository::Repository,
}

impl<'listener> Handler<'listener> {
    pub fn new(
        project_id: String,
        project_store: projects::Storage,
        gb_repository: &'listener gb_repository::Repository,
    ) -> Self {
        Self {
            project_id,
            project_store,
            gb_repository,
        }
    }

    fn get_current_file_content(
        &self,
        project_repository: &project_repository::Repository,
        path: &std::path::Path,
    ) -> Result<Option<String>> {
        if project_repository.is_path_ignored(path)? {
            return Ok(None);
        }

        let reader = project_repository.get_wd_reader();

        let path = path.to_str().unwrap();
        if !reader.exists(path) {
            return Ok(Some(String::new()));
        }

        if reader.size(path)? > 100_000 {
            log::warn!("{}: ignoring large file: {}", self.project_id, path);
            return Ok(None);
        }

        match reader.read(path)? {
            reader::Content::UTF8(content) => Ok(Some(content)),
            reader::Content::Binary(_) => {
                log::warn!("{}: ignoring non-utf8 file: {}", self.project_id, path);
                return Ok(None);
            }
        }
    }

    fn get_latest_file_from_repository_head(
        &self,
        project_repository: &'listener project_repository::Repository,
        path: &str,
    ) -> Result<Option<String>> {
        let head_reader = project_repository
            .get_head_reader()
            .context("failed to get head reader")?;

        if !head_reader.exists(path) {
            return Ok(None);
        }

        if head_reader.size(path)? > 100_000 {
            log::warn!("{}: ignoring large file: {}", self.project_id, path);
            return Ok(None);
        }

        match head_reader.read(path)? {
            reader::Content::UTF8(content) => Ok(Some(content)),
            reader::Content::Binary(_) => {
                log::warn!("{}: ignoring non-utf8 file: {}", self.project_id, path);
                return Ok(None);
            }
        }
    }

    fn get_latest_file_contents(
        &self,
        project_repository: &project_repository::Repository,
        path: &std::path::Path,
    ) -> Result<Option<String>> {
        let path = path.to_str().unwrap();
        match self.gb_repository.git_repository.head() {
            Ok(head) => {
                let head_commit = head.peel_to_commit()?;
                let commit_reader = reader::CommitReader::from_commit(
                    &self.gb_repository.git_repository,
                    head_commit,
                )
                .context("failed to get commit reader")?;

                let session_path = std::path::Path::new("wd").join(path);
                if !commit_reader.exists(session_path.to_str().unwrap()) {
                    return Ok(None);
                }

                if commit_reader.size(session_path.to_str().unwrap())? > 100_000 {
                    log::warn!("{}: ignoring large file: {}", self.project_id, path);
                    return Ok(None);
                }

                match commit_reader.read(session_path.to_str().unwrap())? {
                    reader::Content::UTF8(content) => Ok(Some(content)),
                    reader::Content::Binary(_) => {
                        log::warn!("{}: ignoring non-utf8 file: {}", self.project_id, path);
                        return Ok(None);
                    }
                }
            }
            Err(err) => {
                if err.code() == git2::ErrorCode::UnbornBranch {
                    return self.get_latest_file_from_repository_head(project_repository, path);
                }
                Err(err).context("failed to get head")?
            }
        }
    }

    fn get_current_deltas(&self, path: &std::path::Path) -> Result<Option<Vec<deltas::Delta>>> {
        let current_session = self.gb_repository.get_current_session()?;
        if current_session.is_none() {
            return Ok(None);
        }
        let current_session = current_session.unwrap();
        let reader = self
            .gb_repository
            .get_session_reader(&current_session)
            .context("failed to get session reader")?;
        let deltas = reader
            .file_deltas(path)
            .context("failed to get file deltas")?;
        Ok(deltas)
    }

    pub fn handle<P: AsRef<std::path::Path>>(&self, path: P) -> Result<Vec<events::Event>> {
        let project = self
            .project_store
            .get_project(&self.project_id)
            .with_context(|| "failed to get project")?;

        if project.is_none() {
            return Err(anyhow::anyhow!("project not found"));
        }
        let project = project.unwrap();

        let project_repository = project_repository::Repository::open(&project)
            .with_context(|| "failed to open project repository for project")?;

        let path = path.as_ref();
        let current_file_content = match self
            .get_current_file_content(&project_repository, &path)
            .with_context(|| "failed to get current file content")?
        {
            Some(content) => content,
            None => return Ok(vec![]),
        };

        let latest_file_content = self
            .get_latest_file_contents(&project_repository, &path)
            .with_context(|| "failed to get latest file content")?;

        let current_deltas = self
            .get_current_deltas(&path)
            .with_context(|| "failed to get current deltas")?;

        let mut text_doc = match (latest_file_content, current_deltas) {
            (Some(latest_contents), Some(deltas)) => {
                deltas::TextDocument::new(Some(&latest_contents), deltas)?
            }
            (Some(latest_contents), None) => {
                deltas::TextDocument::new(Some(&latest_contents), vec![])?
            }
            (None, Some(deltas)) => deltas::TextDocument::new(None, deltas)?,
            (None, None) => deltas::TextDocument::new(None, vec![])?,
        };

        if !text_doc.update(&current_file_content)? {
            log::debug!(
                "{}: {} no new deltas, ignoring",
                self.project_id,
                path.display()
            );
            return Ok(vec![]);
        }

        let current_session = self.gb_repository.get_or_create_current_session()?;

        let writer = self.gb_repository.get_session_writer(&current_session)?;

        let deltas = text_doc.get_deltas();

        writer
            .write_deltas(path, &deltas)
            .with_context(|| "failed to write deltas")?;
        writer
            .write_session_wd_file(path, &current_file_content)
            .with_context(|| "failed to write file")?;

        Ok(vec![
            events::Event::Session((project.clone(), current_session.clone())),
            events::Event::Deltas((project, current_session, path.to_path_buf(), deltas)),
        ])
    }
}
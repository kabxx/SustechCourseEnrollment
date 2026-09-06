use crate::error::{AppError, Result};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

const KEYRING_SERVICE: &str = "SustechCourseEnrollment";
const DATA_DIR: &str = "SustechCourseEnrollmentData";
const STORE_FILE: &str = "accounts.json";
const COOKIE_FILE_PREFIX: &str = "cookies-";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    pub target_courses: Vec<String>,
    #[serde(rename = "display_name")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountStore {
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub id: String,
    pub password: String,
}

impl Credentials {
    pub fn new(id: String, password: String) -> Result<Self> {
        let id = id.trim().to_owned();
        validate_id(&id)?;
        if password.is_empty() {
            return Err(AppError::Account("password cannot be empty".into()));
        }
        Ok(Self { id, password })
    }
}

impl AccountStore {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).map_err(|error| {
            AppError::Account(format!("read account store {}: {error}", path.display()))
        })?;
        let store: Self = serde_json::from_str(&text).map_err(|error| {
            AppError::Account(format!("parse account store {}: {error}", path.display()))
        })?;
        store.validate()?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = Self::path()?;
        let parent = path.parent().ok_or_else(|| {
            AppError::Account("account store path has no parent directory".into())
        })?;
        fs::create_dir_all(parent)?;
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&temp, bytes).map_err(|error| {
            AppError::Account(format!("write account store {}: {error}", temp.display()))
        })?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temp, &path).map_err(|error| {
            AppError::Account(format!("replace account store {}: {error}", path.display()))
        })?;
        Ok(())
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join(STORE_FILE))
    }

    pub fn data_dir() -> Result<PathBuf> {
        let executable = std::env::current_exe().map_err(|error| {
            AppError::Account(format!("could not determine executable directory: {error}"))
        })?;
        let parent = executable
            .parent()
            .ok_or_else(|| AppError::Account("executable path has no parent directory".into()))?;
        Ok(parent.join(DATA_DIR))
    }

    pub fn cache_path(id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let path = Self::path()?;
        let parent = path.parent().ok_or_else(|| {
            AppError::Account("account store path has no parent directory".into())
        })?;
        Ok(parent.join(format!("cache-{id}.json")))
    }

    pub fn cookie_path(id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let path = Self::path()?;
        let parent = path.parent().ok_or_else(|| {
            AppError::Account("account store path has no parent directory".into())
        })?;
        Ok(parent.join(format!("{COOKIE_FILE_PREFIX}{id}.json")))
    }

    pub fn add(&mut self, id: String, password: String) -> Result<()> {
        self.add_with_name(id, password, None)
    }

    pub fn add_with_name(
        &mut self,
        id: String,
        password: String,
        name: Option<String>,
    ) -> Result<()> {
        let id = id.trim().to_owned();
        validate_id(&id)?;
        if password.is_empty() {
            return Err(AppError::Account("password cannot be empty".into()));
        }
        if self.accounts.iter().any(|account| account.id == id) {
            return Err(AppError::Account("this account is already added".into()));
        }
        credential(&id)?
            .set_password(&password)
            .map_err(keyring_error)?;
        self.accounts.push(Account {
            id,
            target_courses: Vec::new(),
            name: normalize_name(name),
        });
        if let Err(error) = self.save() {
            let _ = credential(&self.accounts.last().expect("account was pushed").id)
                .and_then(|entry| entry.delete_credential().map_err(keyring_error));
            self.accounts.pop();
            return Err(error);
        }
        Ok(())
    }

    pub fn set_name(&mut self, index: usize, name: Option<String>) -> Result<()> {
        let normalized = normalize_name(name);
        let previous = self
            .accounts
            .get(index)
            .ok_or_else(|| AppError::Account("account index is out of range".into()))?
            .name
            .clone();
        self.accounts[index].name = normalized;
        if let Err(error) = self.save() {
            self.accounts[index].name = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn remove(&mut self, index: usize) -> Result<Account> {
        if index >= self.accounts.len() {
            return Err(AppError::Account("account index is out of range".into()));
        }
        let account = self.accounts.remove(index);
        let previous_password = credential(&account.id)
            .and_then(|entry| entry.get_password().map_err(keyring_error))
            .ok();
        let delete_result = credential(&account.id)
            .and_then(|entry| entry.delete_credential().map_err(keyring_error));
        if let Err(error) = delete_result {
            self.accounts.insert(index, account.clone());
            return Err(error);
        }
        if let Err(error) = self.save() {
            self.accounts.insert(index, account.clone());
            if let Some(password) = previous_password {
                let _ = credential(&account.id)
                    .and_then(|entry| entry.set_password(&password).map_err(keyring_error));
            }
            return Err(error);
        }
        if let Ok(cache_path) = Self::cache_path(&account.id) {
            let _ = fs::remove_file(cache_path);
        }
        if let Ok(cookie_path) = Self::cookie_path(&account.id) {
            let _ = fs::remove_file(cookie_path);
        }
        Ok(account)
    }

    pub fn password(&self, index: usize) -> Result<String> {
        let account = self
            .accounts
            .get(index)
            .ok_or_else(|| AppError::Account("account index is out of range".into()))?;
        let entry = credential(&account.id)?;
        entry.get_password().map_err(keyring_error)
    }

    pub fn credentials(&self, index: usize) -> Result<Credentials> {
        let account = self
            .accounts
            .get(index)
            .ok_or_else(|| AppError::Account("account index is out of range".into()))?;
        Credentials::new(account.id.clone(), self.password(index)?)
    }

    pub fn set_targets(&mut self, index: usize, targets: Vec<String>) -> Result<()> {
        let account = self
            .accounts
            .get_mut(index)
            .ok_or_else(|| AppError::Account("account index is out of range".into()))?;
        let mut clean = Vec::with_capacity(targets.len());
        for target in targets {
            let target = target.trim().to_owned();
            if target.is_empty() {
                continue;
            }
            if !clean.iter().any(|existing| existing == &target) {
                clean.push(target);
            }
        }
        account.target_courses = clean;
        self.save()
    }

    fn validate(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for account in &self.accounts {
            validate_id(&account.id)?;
            if !ids.insert(&account.id) {
                return Err(AppError::Account(format!(
                    "duplicate account id: {}",
                    account.id
                )));
            }
            let mut targets = std::collections::BTreeSet::new();
            for target in &account.target_courses {
                if target.trim().is_empty() || !targets.insert(target.trim()) {
                    return Err(AppError::Account(format!(
                        "invalid or duplicate target course for {}",
                        account.id
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(AppError::Account("student ID cannot be empty".into()));
    }
    if id.len() > 128
        || id.chars().any(|ch| {
            ch.is_control() || matches!(ch, ':' | '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(AppError::Account(
            "student ID contains invalid characters".into(),
        ));
    }
    Ok(())
}

fn normalize_name(name: Option<String>) -> Option<String> {
    let name = name?
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    (!name.is_empty()).then_some(name)
}

fn credential(id: &str) -> Result<Entry> {
    Entry::new(KEYRING_SERVICE, id).map_err(keyring_error)
}

fn keyring_error(error: keyring::Error) -> AppError {
    AppError::Account(format!("system credential store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_accounts_and_deduplicates_targets() {
        let mut store = AccountStore {
            accounts: vec![Account {
                id: "123".into(),
                target_courses: vec![],
                name: None,
            }],
        };
        store
            .set_targets(0, vec!["A".into(), " A ".into(), "".into()])
            .unwrap();
        assert_eq!(store.accounts[0].target_courses, vec!["A"]);
        assert!(store.set_targets(2, vec!["A".into()]).is_err());
    }

    #[test]
    fn rejects_path_like_ids() {
        assert!(validate_id("C:\\secret").is_err());
        assert!(validate_id("id:2026").is_err());
        assert!(validate_id("id*2026").is_err());
        assert!(validate_id("").is_err());
        assert!(validate_id("2026-01").is_ok());
    }

    #[test]
    fn cache_path_is_scoped_to_application_data() {
        let path = AccountStore::cache_path("123").unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("cache-123"));
        assert!(path.parent().unwrap().ends_with(DATA_DIR));
    }

    #[test]
    fn data_path_is_next_to_test_executable() {
        let executable_parent = std::env::current_exe().unwrap();
        let executable_parent = executable_parent.parent().unwrap();
        let data_dir = AccountStore::data_dir().unwrap();
        assert_eq!(data_dir.parent().unwrap(), executable_parent);
        assert_eq!(data_dir.file_name().unwrap(), DATA_DIR);
    }

    #[test]
    fn reads_current_account_shape() {
        let named: Account =
            serde_json::from_str(r#"{"id":"123","target_courses":[],"display_name":"张三"}"#)
                .unwrap();
        assert_eq!(named.name.as_deref(), Some("张三"));
        assert!(serde_json::from_str::<Account>(
            r#"{"id":"123","target_courses":[],"name":"李四"}"#
        )
        .is_err());
    }

    #[test]
    fn normalizes_optional_names() {
        assert_eq!(normalize_name(None), None);
        assert_eq!(normalize_name(Some("  张三\n".into())), Some("张三".into()));
        assert_eq!(normalize_name(Some(" \t ".into())), None);
    }
}

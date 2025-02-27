// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use super::path_to_regex::parse;
use super::path_to_regex::string_to_regex;
use super::path_to_regex::Compiler;
use super::path_to_regex::Key;
use super::path_to_regex::MatchResult;
use super::path_to_regex::Matcher;
use super::path_to_regex::StringOrNumber;
use super::path_to_regex::StringOrVec;
use super::path_to_regex::Token;

use crate::deno_dir;
use crate::file_fetcher::CacheSetting;
use crate::file_fetcher::FileFetcher;
use crate::http_cache::HttpCache;

use deno_core::anyhow::anyhow;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::resolve_url;
use deno_core::serde::Deserialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::url::Position;
use deno_core::url::Url;
use deno_core::ModuleSpecifier;
use deno_runtime::deno_web::BlobStore;
use deno_runtime::permissions::Permissions;
use log::error;
use lspower::lsp;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

const CONFIG_PATH: &str = "/.well-known/deno-import-intellisense.json";
const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
  .add(b' ')
  .add(b'"')
  .add(b'#')
  .add(b'<')
  .add(b'>')
  .add(b'?')
  .add(b'`')
  .add(b'{')
  .add(b'}')
  .add(b'/')
  .add(b':')
  .add(b';')
  .add(b'=')
  .add(b'@')
  .add(b'[')
  .add(b'\\')
  .add(b']')
  .add(b'^')
  .add(b'|')
  .add(b'$')
  .add(b'&')
  .add(b'+')
  .add(b',');

lazy_static::lazy_static! {
  static ref REPLACEMENT_VARIABLE_RE: Regex =
    Regex::new(r"\$\{\{?(\w+)\}?\}").unwrap();
}

fn base_url(url: &Url) -> String {
  url.origin().ascii_serialization()
}

#[derive(Debug)]
enum CompletorType {
  Literal(String),
  Key {
    key: Key,
    prefix: Option<String>,
    index: usize,
  },
}

/// Determine if a completion at a given offset is a string literal or a key/
/// variable.
fn get_completor_type(
  offset: usize,
  tokens: &[Token],
  match_result: &MatchResult,
) -> Option<CompletorType> {
  let mut len = 0_usize;
  for (index, token) in tokens.iter().enumerate() {
    match token {
      Token::String(s) => {
        len += s.chars().count();
        if offset < len {
          return Some(CompletorType::Literal(s.clone()));
        }
      }
      Token::Key(k) => {
        if let Some(prefix) = &k.prefix {
          len += prefix.chars().count();
          if offset < len {
            return Some(CompletorType::Key {
              key: k.clone(),
              prefix: Some(prefix.clone()),
              index,
            });
          }
        }
        if offset < len {
          return None;
        }
        if let StringOrNumber::String(name) = &k.name {
          let value = match_result
            .get(name)
            .map(|s| s.to_string(Some(k)))
            .unwrap_or_default();
          len += value.chars().count();
          if offset <= len {
            return Some(CompletorType::Key {
              key: k.clone(),
              prefix: None,
              index,
            });
          }
        }
        if let Some(suffix) = &k.suffix {
          len += suffix.chars().count();
          if offset <= len {
            return Some(CompletorType::Literal(suffix.clone()));
          }
        }
      }
    }
  }

  None
}

/// Convert a completion URL string from a completions configuration into a
/// fully qualified URL which can be fetched to provide the completions.
fn get_completion_endpoint(
  url: &str,
  tokens: &[Token],
  match_result: &MatchResult,
) -> Result<ModuleSpecifier, AnyError> {
  let mut url_str = url.to_string();
  for (key, value) in match_result.params.iter() {
    if let StringOrNumber::String(name) = key {
      let maybe_key = tokens.iter().find_map(|t| match t {
        Token::Key(k) if k.name == *key => Some(k),
        _ => None,
      });
      url_str =
        url_str.replace(&format!("${{{}}}", name), &value.to_string(maybe_key));
      url_str = url_str.replace(
        &format!("${{{{{}}}}}", name),
        &percent_encoding::percent_encode(
          value.to_string(maybe_key).as_bytes(),
          COMPONENT,
        )
        .to_string(),
      );
    }
  }
  resolve_url(&url_str).map_err(|err| err.into())
}

fn parse_replacement_variables<S: AsRef<str>>(s: S) -> Vec<String> {
  REPLACEMENT_VARIABLE_RE
    .captures_iter(s.as_ref())
    .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
    .collect()
}

/// Validate a registry configuration JSON structure.
fn validate_config(config: &RegistryConfigurationJson) -> Result<(), AnyError> {
  if config.version != 1 {
    return Err(anyhow!(
      "Invalid registry configuration. Expected version 1 got {}.",
      config.version
    ));
  }
  for registry in &config.registries {
    let (_, keys) = string_to_regex(&registry.schema, None)?;
    let key_names: Vec<String> = keys.map_or_else(Vec::new, |keys| {
      keys
        .iter()
        .filter_map(|k| {
          if let StringOrNumber::String(s) = &k.name {
            Some(s.clone())
          } else {
            None
          }
        })
        .collect()
    });

    for key_name in &key_names {
      if !registry
        .variables
        .iter()
        .map(|var| var.key.to_owned())
        .any(|x| x == *key_name)
      {
        return Err(anyhow!("Invalid registry configuration. Registry with schema \"{}\" is missing variable declaration for key \"{}\".", registry.schema, key_name));
      }
    }

    for variable in &registry.variables {
      let key_index = key_names.iter().position(|key| *key == variable.key);
      let key_index = key_index.ok_or_else(||anyhow!("Invalid registry configuration. Registry with schema \"{}\" is missing a path parameter in schema for variable \"{}\".", registry.schema, variable.key))?;

      let replacement_variables = parse_replacement_variables(&variable.url);
      let limited_keys = key_names.get(0..key_index).unwrap();
      for v in replacement_variables {
        if variable.key == v {
          return Err(anyhow!("Invalid registry configuration. Url \"{}\" (for variable \"{}\" in registry with schema \"{}\") uses variable \"{}\", which is not allowed because that would be a self reference.", variable.url, variable.key, registry.schema, v));
        }

        let key_index = limited_keys.iter().position(|key| key == &v);

        if key_index.is_none() {
          return Err(anyhow!("Invalid registry configuration. Url \"{}\" (for variable \"{}\" in registry with schema \"{}\") uses variable \"{}\", which is not allowed because the schema defines \"{}\" to the right of \"{}\".", variable.url, variable.key, registry.schema, v, v, variable.key));
        }
      }
    }
  }

  Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegistryConfigurationVariable {
  /// The name of the variable.
  key: String,
  /// The URL with variable substitutions of the endpoint that will provide
  /// completions for the variable.
  url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegistryConfiguration {
  /// A Express-like path which describes how URLs are composed for a registry.
  schema: String,
  /// The variables denoted in the `schema` should have a variable entry.
  variables: Vec<RegistryConfigurationVariable>,
}

impl RegistryConfiguration {
  fn get_url_for_key(&self, key: &Key) -> Option<&str> {
    self.variables.iter().find_map(|v| {
      if key.name == StringOrNumber::String(v.key.clone()) {
        Some(v.url.as_str())
      } else {
        None
      }
    })
  }
}

/// A structure that represents the configuration of an origin and its module
/// registries.
#[derive(Debug, Deserialize)]
struct RegistryConfigurationJson {
  version: u32,
  registries: Vec<RegistryConfiguration>,
}

/// A structure which holds the information about currently configured module
/// registries and can provide completion information for URLs that match
/// one of the enabled registries.
#[derive(Debug, Clone)]
pub struct ModuleRegistry {
  origins: HashMap<String, Vec<RegistryConfiguration>>,
  file_fetcher: FileFetcher,
}

impl Default for ModuleRegistry {
  fn default() -> Self {
    // This only gets used when creating the tsc runtime and for testing, and so
    // it shouldn't ever actually access the DenoDir, so it doesn't support a
    // custom root.
    let dir = deno_dir::DenoDir::new(None).unwrap();
    let location = dir.root.join("registries");
    let http_cache = HttpCache::new(&location);
    let cache_setting = CacheSetting::RespectHeaders;
    let file_fetcher = FileFetcher::new(
      http_cache,
      cache_setting,
      true,
      None,
      BlobStore::default(),
      None,
    )
    .unwrap();

    Self {
      origins: HashMap::new(),
      file_fetcher,
    }
  }
}

impl ModuleRegistry {
  pub fn new(location: &Path) -> Self {
    let http_cache = HttpCache::new(location);
    let file_fetcher = FileFetcher::new(
      http_cache,
      CacheSetting::RespectHeaders,
      true,
      None,
      BlobStore::default(),
      None,
    )
    .context("Error creating file fetcher in module registry.")
    .unwrap();

    Self {
      origins: HashMap::new(),
      file_fetcher,
    }
  }

  fn complete_literal(
    &self,
    s: String,
    completions: &mut HashMap<String, lsp::CompletionItem>,
    current_specifier: &str,
    offset: usize,
    range: &lsp::Range,
  ) {
    let label = if s.starts_with('/') {
      s[0..].to_string()
    } else {
      s.to_string()
    };
    let full_text = format!(
      "{}{}{}",
      &current_specifier[..offset],
      s,
      &current_specifier[offset..]
    );
    let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
      range: *range,
      new_text: full_text.clone(),
    }));
    let filter_text = Some(full_text);
    completions.insert(
      s,
      lsp::CompletionItem {
        label,
        kind: Some(lsp::CompletionItemKind::FOLDER),
        filter_text,
        sort_text: Some("1".to_string()),
        text_edit,
        ..Default::default()
      },
    );
  }

  /// Disable a registry, removing its configuration, if any, from memory.
  pub async fn disable(&mut self, origin: &str) -> Result<(), AnyError> {
    let origin = base_url(&Url::parse(origin)?);
    self.origins.remove(&origin);
    Ok(())
  }

  /// Check to see if the given origin has a registry configuration.
  pub(crate) async fn check_origin(
    &self,
    origin: &str,
  ) -> Result<(), AnyError> {
    let origin_url = Url::parse(origin)?;
    let specifier = origin_url.join(CONFIG_PATH)?;
    self.fetch_config(&specifier).await?;
    Ok(())
  }

  /// Fetch and validate the specifier to a registry configuration, resolving
  /// with the configuration if valid.
  async fn fetch_config(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Vec<RegistryConfiguration>, AnyError> {
    let fetch_result = self
      .file_fetcher
      .fetch(specifier, &mut Permissions::allow_all())
      .await;
    // if there is an error fetching, we will cache an empty file, so that
    // subsequent requests they are just an empty doc which will error without
    // needing to connect to the remote URL. We will cache it for 1 week.
    if fetch_result.is_err() {
      let mut headers_map = HashMap::new();
      headers_map.insert(
        "cache-control".to_string(),
        "max-age=604800, immutable".to_string(),
      );
      self
        .file_fetcher
        .http_cache
        .set(specifier, headers_map, &[])?;
    }
    let file = fetch_result?;
    let config: RegistryConfigurationJson = serde_json::from_str(&file.source)?;
    validate_config(&config)?;
    Ok(config.registries)
  }

  /// Enable a registry by attempting to retrieve its configuration and
  /// validating it.
  pub async fn enable(&mut self, origin: &str) -> Result<(), AnyError> {
    let origin_url = Url::parse(origin)?;
    let origin = base_url(&origin_url);
    #[allow(clippy::map_entry)]
    // we can't use entry().or_insert_with() because we can't use async closures
    if !self.origins.contains_key(&origin) {
      let specifier = origin_url.join(CONFIG_PATH)?;
      let configs = self.fetch_config(&specifier).await?;
      self.origins.insert(origin, configs);
    }

    Ok(())
  }

  #[cfg(test)]
  /// This is only used during testing, as it directly provides the full URL
  /// for obtaining the registry configuration, versus "guessing" at it.
  async fn enable_custom(&mut self, specifier: &str) -> Result<(), AnyError> {
    let specifier = Url::parse(specifier)?;
    let origin = base_url(&specifier);
    #[allow(clippy::map_entry)]
    if !self.origins.contains_key(&origin) {
      let configs = self.fetch_config(&specifier).await?;
      self.origins.insert(origin, configs);
    }

    Ok(())
  }

  /// For a string specifier from the client, provide a set of completions, if
  /// any, for the specifier.
  pub(crate) async fn get_completions(
    &self,
    current_specifier: &str,
    offset: usize,
    range: &lsp::Range,
    specifier_exists: impl Fn(&ModuleSpecifier) -> bool,
  ) -> Option<Vec<lsp::CompletionItem>> {
    if let Ok(specifier) = Url::parse(current_specifier) {
      let origin = base_url(&specifier);
      let origin_len = origin.chars().count();
      if offset >= origin_len {
        if let Some(registries) = self.origins.get(&origin) {
          let path = &specifier[Position::BeforePath..];
          let path_offset = offset - origin_len;
          let mut completions = HashMap::<String, lsp::CompletionItem>::new();
          let mut did_match = false;
          for registry in registries {
            let tokens = parse(&registry.schema, None)
              .map_err(|e| {
                error!(
                  "Error parsing registry schema for origin \"{}\". {}",
                  origin, e
                );
              })
              .ok()?;
            let mut i = tokens.len();
            let last_key_name =
              StringOrNumber::String(tokens.iter().last().map_or_else(
                || "".to_string(),
                |t| {
                  if let Token::Key(key) = t {
                    if let StringOrNumber::String(s) = &key.name {
                      return s.clone();
                    }
                  }
                  "".to_string()
                },
              ));
            loop {
              let matcher = Matcher::new(&tokens[..i], None)
                .map_err(|e| {
                  error!(
                    "Error creating matcher for schema for origin \"{}\". {}",
                    origin, e
                  );
                })
                .ok()?;
              if let Some(match_result) = matcher.matches(path) {
                did_match = true;
                let completor_type =
                  get_completor_type(path_offset, &tokens, &match_result);
                match completor_type {
                  Some(CompletorType::Literal(s)) => self.complete_literal(
                    s,
                    &mut completions,
                    current_specifier,
                    offset,
                    range,
                  ),
                  Some(CompletorType::Key { key, prefix, index }) => {
                    let maybe_url = registry.get_url_for_key(&key);
                    if let Some(url) = maybe_url {
                      if let Some(items) = self
                        .get_variable_items(url, &tokens, &match_result)
                        .await
                      {
                        let compiler = Compiler::new(&tokens[..=index], None);
                        let base = Url::parse(&origin).ok()?;
                        for (idx, item) in items.into_iter().enumerate() {
                          let label = if let Some(p) = &prefix {
                            format!("{}{}", p, item)
                          } else {
                            item.clone()
                          };
                          let kind = if key.name == last_key_name {
                            Some(lsp::CompletionItemKind::FILE)
                          } else {
                            Some(lsp::CompletionItemKind::FOLDER)
                          };
                          let mut params = match_result.params.clone();
                          params.insert(
                            key.name.clone(),
                            StringOrVec::from_str(&item, &key),
                          );
                          let path =
                            compiler.to_path(&params).unwrap_or_default();
                          let item_specifier = base.join(&path).ok()?;
                          let full_text = item_specifier.as_str();
                          let text_edit = Some(lsp::CompletionTextEdit::Edit(
                            lsp::TextEdit {
                              range: *range,
                              new_text: full_text.to_string(),
                            },
                          ));
                          let command = if key.name == last_key_name
                            && !specifier_exists(&item_specifier)
                          {
                            Some(lsp::Command {
                              title: "".to_string(),
                              command: "deno.cache".to_string(),
                              arguments: Some(vec![json!([item_specifier])]),
                            })
                          } else {
                            None
                          };
                          let detail = Some(format!("({})", key.name));
                          let filter_text = Some(full_text.to_string());
                          let sort_text = Some(format!("{:0>10}", idx + 1));
                          completions.insert(
                            item,
                            lsp::CompletionItem {
                              label,
                              kind,
                              detail,
                              sort_text,
                              filter_text,
                              text_edit,
                              command,
                              ..Default::default()
                            },
                          );
                        }
                      }
                    }
                  }
                  None => (),
                }
                break;
              }
              i -= 1;
              // If we have fallen though to the first token, and we still
              // didn't get a match
              if i == 0 {
                match &tokens[i] {
                  // so if the first token is a string literal, we will return
                  // that as a suggestion
                  Token::String(s) => {
                    if s.starts_with(path) {
                      let label = s.to_string();
                      let kind = Some(lsp::CompletionItemKind::FOLDER);
                      let mut url = specifier.clone();
                      url.set_path(s);
                      let full_text = url.as_str();
                      let text_edit =
                        Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                          range: *range,
                          new_text: full_text.to_string(),
                        }));
                      let filter_text = Some(full_text.to_string());
                      completions.insert(
                        s.to_string(),
                        lsp::CompletionItem {
                          label,
                          kind,
                          filter_text,
                          sort_text: Some("1".to_string()),
                          text_edit,
                          ..Default::default()
                        },
                      );
                    }
                  }
                  // if the token though is a key, and the key has a prefix, and
                  // the path matches the prefix, we will go and get the items
                  // for that first key and return them.
                  Token::Key(k) => {
                    if let Some(prefix) = &k.prefix {
                      let maybe_url = registry.get_url_for_key(k);
                      if let Some(url) = maybe_url {
                        if let Some(items) = self.get_items(url).await {
                          let base = Url::parse(&origin).ok()?;
                          for (idx, item) in items.into_iter().enumerate() {
                            let path = format!("{}{}", prefix, item);
                            let kind = Some(lsp::CompletionItemKind::FOLDER);
                            let item_specifier = base.join(&path).ok()?;
                            let full_text = item_specifier.as_str();
                            let text_edit = Some(
                              lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                                range: *range,
                                new_text: full_text.to_string(),
                              }),
                            );
                            let command = if k.name == last_key_name
                              && !specifier_exists(&item_specifier)
                            {
                              Some(lsp::Command {
                                title: "".to_string(),
                                command: "deno.cache".to_string(),
                                arguments: Some(vec![json!([item_specifier])]),
                              })
                            } else {
                              None
                            };
                            let detail = Some(format!("({})", k.name));
                            let filter_text = Some(full_text.to_string());
                            let sort_text = Some(format!("{:0>10}", idx + 1));
                            completions.insert(
                              item.clone(),
                              lsp::CompletionItem {
                                label: item,
                                kind,
                                detail,
                                sort_text,
                                filter_text,
                                text_edit,
                                command,
                                ..Default::default()
                              },
                            );
                          }
                        }
                      }
                    }
                  }
                }
                break;
              }
            }
          }
          // If we return None, other sources of completions will be looked for
          // but if we did at least match part of a registry, we should send an
          // empty vector so that no-completions will be sent back to the client
          return if completions.is_empty() && !did_match {
            None
          } else {
            Some(completions.into_iter().map(|(_, i)| i).collect())
          };
        }
      }
    }

    self.get_origin_completions(current_specifier, range)
  }

  pub fn get_origin_completions(
    &self,
    current_specifier: &str,
    range: &lsp::Range,
  ) -> Option<Vec<lsp::CompletionItem>> {
    let items = self
      .origins
      .keys()
      .filter_map(|k| {
        let mut origin = k.as_str().to_string();
        if origin.ends_with('/') {
          origin.pop();
        }
        if origin.starts_with(current_specifier) {
          let text_edit = Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
            range: *range,
            new_text: origin.clone(),
          }));
          Some(lsp::CompletionItem {
            label: origin,
            kind: Some(lsp::CompletionItemKind::FOLDER),
            detail: Some("(registry)".to_string()),
            sort_text: Some("2".to_string()),
            text_edit,
            ..Default::default()
          })
        } else {
          None
        }
      })
      .collect::<Vec<lsp::CompletionItem>>();
    if !items.is_empty() {
      Some(items)
    } else {
      None
    }
  }

  async fn get_items(&self, url: &str) -> Option<Vec<String>> {
    let specifier = ModuleSpecifier::parse(url).ok()?;
    let file = self
      .file_fetcher
      .fetch(&specifier, &mut Permissions::allow_all())
      .await
      .map_err(|err| {
        error!(
          "Internal error fetching endpoint \"{}\". {}",
          specifier, err
        );
      })
      .ok()?;
    let items: Vec<String> = serde_json::from_str(&file.source)
      .map_err(|err| {
        error!(
          "Error parsing response from endpoint \"{}\". {}",
          specifier, err
        );
      })
      .ok()?;
    Some(items)
  }

  async fn get_variable_items(
    &self,
    url: &str,
    tokens: &[Token],
    match_result: &MatchResult,
  ) -> Option<Vec<String>> {
    let specifier = get_completion_endpoint(url, tokens, match_result)
      .map_err(|err| {
        error!("Internal error mapping endpoint \"{}\". {}", url, err);
      })
      .ok()?;
    let file = self
      .file_fetcher
      .fetch(&specifier, &mut Permissions::allow_all())
      .await
      .map_err(|err| {
        error!(
          "Internal error fetching endpoint \"{}\". {}",
          specifier, err
        );
      })
      .ok()?;
    let items: Vec<String> = serde_json::from_str(&file.source)
      .map_err(|err| {
        error!(
          "Error parsing response from endpoint \"{}\". {}",
          specifier, err
        );
      })
      .ok()?;
    Some(items)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  #[test]
  fn test_validate_registry_configuration() {
    assert!(validate_config(&RegistryConfigurationJson {
      version: 2,
      registries: vec![],
    })
    .is_err());

    let cfg = RegistryConfigurationJson {
      version: 1,
      registries: vec![RegistryConfiguration {
        schema: "/:module@:version/:path*".to_string(),
        variables: vec![
          RegistryConfigurationVariable {
            key: "module".to_string(),
            url: "https://api.deno.land/modules?short".to_string(),
          },
          RegistryConfigurationVariable {
            key: "version".to_string(),
            url: "https://deno.land/_vsc1/module/${module}".to_string(),
          },
        ],
      }],
    };
    assert!(validate_config(&cfg).is_err());

    let cfg = RegistryConfigurationJson {
      version: 1,
      registries: vec![RegistryConfiguration {
        schema: "/:module@:version/:path*".to_string(),
        variables: vec![
          RegistryConfigurationVariable {
            key: "module".to_string(),
            url: "https://api.deno.land/modules?short".to_string(),
          },
          RegistryConfigurationVariable {
            key: "version".to_string(),
            url: "https://deno.land/_vsc1/module/${module}/${path}".to_string(),
          },
          RegistryConfigurationVariable {
            key: "path".to_string(),
            url: "https://deno.land/_vsc1/module/${module}/v/${{version}}"
              .to_string(),
          },
        ],
      }],
    };
    assert!(validate_config(&cfg).is_err());

    let cfg = RegistryConfigurationJson {
      version: 1,
      registries: vec![RegistryConfiguration {
        schema: "/:module@:version/:path*".to_string(),
        variables: vec![
          RegistryConfigurationVariable {
            key: "module".to_string(),
            url: "https://api.deno.land/modules?short".to_string(),
          },
          RegistryConfigurationVariable {
            key: "version".to_string(),
            url: "https://deno.land/_vsc1/module/${module}/v/${{version}}"
              .to_string(),
          },
          RegistryConfigurationVariable {
            key: "path".to_string(),
            url: "https://deno.land/_vsc1/module/${module}/v/${{version}}"
              .to_string(),
          },
        ],
      }],
    };
    assert!(validate_config(&cfg).is_err());

    let cfg = RegistryConfigurationJson {
      version: 1,
      registries: vec![RegistryConfiguration {
        schema: "/:module@:version/:path*".to_string(),
        variables: vec![
          RegistryConfigurationVariable {
            key: "module".to_string(),
            url: "https://api.deno.land/modules?short".to_string(),
          },
          RegistryConfigurationVariable {
            key: "version".to_string(),
            url: "https://deno.land/_vsc1/module/${module}".to_string(),
          },
          RegistryConfigurationVariable {
            key: "path".to_string(),
            url: "https://deno.land/_vsc1/module/${module}/v/${{version}}"
              .to_string(),
          },
        ],
      }],
    };
    validate_config(&cfg).unwrap();
  }

  #[tokio::test]
  async fn test_registry_completions_origin_match() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let mut module_registry = ModuleRegistry::new(&location);
    module_registry
      .enable("http://localhost:4545/")
      .await
      .expect("could not enable");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 21,
      },
    };
    let completions = module_registry
      .get_completions("h", 1, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "http://localhost:4545");
    assert_eq!(
      completions[0].text_edit,
      Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
        range,
        new_text: "http://localhost:4545".to_string()
      }))
    );
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 36,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost", 16, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "http://localhost:4545");
    assert_eq!(
      completions[0].text_edit,
      Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
        range,
        new_text: "http://localhost:4545".to_string()
      }))
    );
  }

  #[tokio::test]
  async fn test_registry_completions() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let mut module_registry = ModuleRegistry::new(&location);
    module_registry
      .enable("http://localhost:4545/")
      .await
      .expect("could not enable");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 41,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545", 21, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "/x");
    assert_eq!(
      completions[0].text_edit,
      Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
        range,
        new_text: "http://localhost:4545/x".to_string()
      }))
    );
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 42,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/", 22, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "/x");
    assert_eq!(
      completions[0].text_edit,
      Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
        range,
        new_text: "http://localhost:4545/x".to_string()
      }))
    );
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 44,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/x/", 24, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 2);
    assert!(completions[0].label == *"a" || completions[0].label == *"b");
    assert!(completions[1].label == *"a" || completions[1].label == *"b");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 46,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/x/a@", 26, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 3);
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 53,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/x/a@v1.0.0/", 33, &range, |_| {
        false
      })
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 2);
    assert_eq!(completions[0].detail, Some("(path)".to_string()));
    assert_eq!(completions[0].kind, Some(lsp::CompletionItemKind::FILE));
    assert!(completions[0].command.is_some());
    assert_eq!(completions[1].detail, Some("(path)".to_string()));
    assert_eq!(completions[0].kind, Some(lsp::CompletionItemKind::FILE));
    assert!(completions[1].command.is_some());
  }

  #[tokio::test]
  async fn test_registry_completions_key_first() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let mut module_registry = ModuleRegistry::new(&location);
    module_registry
      .enable_custom("http://localhost:4545/lsp/registries/deno-import-intellisense-key-first.json")
      .await
      .expect("could not enable");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 42,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/", 22, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 3);
    for completion in completions {
      assert!(completion.text_edit.is_some());
      if let lsp::CompletionTextEdit::Edit(edit) = completion.text_edit.unwrap()
      {
        assert_eq!(
          edit.new_text,
          format!("http://localhost:4545/{}", completion.label)
        );
      } else {
        unreachable!("unexpected text edit");
      }
    }

    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 46,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/cde@", 26, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 2);
    for completion in completions {
      assert!(completion.text_edit.is_some());
      if let lsp::CompletionTextEdit::Edit(edit) = completion.text_edit.unwrap()
      {
        assert_eq!(
          edit.new_text,
          format!("http://localhost:4545/cde@{}", completion.label)
        );
      } else {
        unreachable!("unexpected text edit");
      }
    }
  }

  #[tokio::test]
  async fn test_registry_completions_complex() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let mut module_registry = ModuleRegistry::new(&location);
    module_registry
      .enable_custom("http://localhost:4545/lsp/registries/deno-import-intellisense-complex.json")
      .await
      .expect("could not enable");
    let range = lsp::Range {
      start: lsp::Position {
        line: 0,
        character: 20,
      },
      end: lsp::Position {
        line: 0,
        character: 42,
      },
    };
    let completions = module_registry
      .get_completions("http://localhost:4545/", 22, &range, |_| false)
      .await;
    assert!(completions.is_some());
    let completions = completions.unwrap();
    assert_eq!(completions.len(), 3);
    for completion in completions {
      assert!(completion.text_edit.is_some());
      if let lsp::CompletionTextEdit::Edit(edit) = completion.text_edit.unwrap()
      {
        assert_eq!(
          edit.new_text,
          format!("http://localhost:4545/{}", completion.label)
        );
      } else {
        unreachable!("unexpected text edit");
      }
    }
  }

  #[test]
  fn test_parse_replacement_variables() {
    let actual = parse_replacement_variables(
      "https://deno.land/_vsc1/modules/${module}/v/${{version}}",
    );
    assert_eq!(actual.len(), 2);
    assert!(actual.contains(&"module".to_owned()));
    assert!(actual.contains(&"version".to_owned()));
  }

  #[tokio::test]
  async fn test_check_origin_supported() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let module_registry = ModuleRegistry::new(&location);
    let result = module_registry.check_origin("http://localhost:4545").await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_check_origin_not_supported() {
    let _g = test_util::http_server();
    let temp_dir = TempDir::new().expect("could not create tmp");
    let location = temp_dir.path().join("registries");
    let module_registry = ModuleRegistry::new(&location);
    let result = module_registry.check_origin("https://deno.com").await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err
      .contains("https://deno.com/.well-known/deno-import-intellisense.json"));

    // because we are caching an empty file when we hit an error with import
    // detection when fetching the config file, we should have an error now that
    // indicates trying to parse an empty file.
    let result = module_registry.check_origin("https://deno.com").await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("EOF while parsing a value at line 1 column 0"));
  }
}

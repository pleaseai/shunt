use std::fmt::Write as _;

use anyhow::{anyhow, bail};
use clap::ValueEnum;
use url::Url;

const RESEARCH_URL: &str = "{{RESEARCH_URL}}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AddKind {
    Upstream,
    Provider,
}

impl AddKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Provider => "provider",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Upstream => "wire a provider into a shunt gateway config",
            Self::Provider => "implement support for a new provider in the shunt codebase",
        }
    }
}

struct Blueprint {
    kind: AddKind,
    slug: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    body: &'static str,
}

/// Declare an upstream [`Blueprint`] from its slug, aliases, and description.
///
/// The Markdown body is resolved from the slug (`blueprints/upstream/<slug>.md`),
/// keeping each registry entry a single line so the table stays readable and the
/// slug-to-file mapping cannot drift.
macro_rules! upstream_blueprint {
    ($slug:literal, $aliases:expr, $description:literal) => {
        Blueprint {
            kind: AddKind::Upstream,
            slug: $slug,
            aliases: $aliases,
            description: $description,
            body: include_str!(concat!("../blueprints/upstream/", $slug, ".md")),
        }
    };
}

const BLUEPRINTS: &[Blueprint] = &[
    upstream_blueprint!(
        "anthropic",
        &["claude"],
        "Anthropic API — passthrough or pooled Claude OAuth accounts"
    ),
    upstream_blueprint!(
        "codex",
        &["chatgpt"],
        "ChatGPT/Codex backend via chatgpt_oauth"
    ),
    upstream_blueprint!("openai", &[], "OpenAI Responses API via OPENAI_API_KEY"),
    upstream_blueprint!("xai", &[], "xAI API via XAI_API_KEY"),
    upstream_blueprint!("grok", &[], "SuperGrok subscription via xai_oauth login"),
    upstream_blueprint!(
        "kimi",
        &["moonshot"],
        "Moonshot Kimi (Anthropic-compatible) via MOONSHOT_API_KEY"
    ),
    upstream_blueprint!("cursor", &[], "Cursor subscription via cursor_oauth login"),
];

const GENERIC_UPSTREAM: &str = include_str!("../blueprints/upstream/_generic.md");
const GENERIC_PROVIDER: &str = include_str!("../blueprints/provider/_generic.md");

pub fn list() -> String {
    let mut output = String::from("Available blueprints:\n\n");
    for kind in [AddKind::Upstream, AddKind::Provider] {
        write_kind(&mut output, kind);
        output.push('\n');
    }
    output.push_str("Retrieve one with: shunt add <kind> <name-or-url> [--print]\n");
    output.push_str("Example: shunt add upstream kimi --print | claude\n");
    output
}

pub fn list_kind(kind: AddKind) -> String {
    let mut output = String::from("Available blueprints:\n\n");
    write_kind(&mut output, kind);
    output.push('\n');
    writeln!(
        output,
        "Retrieve one with: shunt add {} <name-or-url> [--print]",
        kind.as_str()
    )
    .expect("writing to a String cannot fail");
    match kind {
        AddKind::Upstream => output.push_str("Example: shunt add upstream kimi --print | claude\n"),
        AddKind::Provider => output.push_str(
            "Example: shunt add provider https://example.com/api-docs --print | claude\n",
        ),
    }
    output
}

pub fn resolve(kind: AddKind, name_or_url: &str) -> anyhow::Result<String> {
    if has_http_scheme(name_or_url) {
        let url = parse_absolute_http_url(name_or_url)
            .map_err(|reason| anyhow!("invalid research URL: {reason}"))?;
        return Ok(generic(kind).replace(RESEARCH_URL, url.as_str()));
    }

    if let Some(blueprint) = BLUEPRINTS.iter().find(|blueprint| {
        blueprint.kind == kind
            && (blueprint.slug == name_or_url || blueprint.aliases.contains(&name_or_url))
    }) {
        return Ok(blueprint.body.to_owned());
    }

    let slugs = available_slugs(kind);
    bail!(
        "unknown {} blueprint {name_or_url:?}; available: {slugs}. An absolute http:// or https:// URL is also accepted",
        kind.as_str()
    )
}

fn generic(kind: AddKind) -> &'static str {
    match kind {
        AddKind::Upstream => GENERIC_UPSTREAM,
        AddKind::Provider => GENERIC_PROVIDER,
    }
}

fn has_http_scheme(value: &str) -> bool {
    value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn parse_absolute_http_url(value: &str) -> Result<Url, String> {
    if value.chars().any(char::is_whitespace) {
        return Err("whitespace is not allowed".to_owned());
    }

    let authority = value
        .find("://")
        .and_then(|delimiter| value.get(delimiter + 3..))
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .unwrap_or_default();
    if authority.is_empty() {
        return Err("missing host".to_owned());
    }

    let url = Url::parse(value).map_err(|error| error.to_string())?;
    if url.username().is_empty() && url.password().is_none() {
        if url.host().is_some() {
            Ok(url)
        } else {
            Err("missing host".to_owned())
        }
    } else {
        Err("credentials are not allowed".to_owned())
    }
}

fn available_slugs(kind: AddKind) -> String {
    let slugs: Vec<_> = BLUEPRINTS
        .iter()
        .filter(|blueprint| blueprint.kind == kind)
        .map(|blueprint| blueprint.slug)
        .collect();
    if slugs.is_empty() {
        "none (URL only)".to_owned()
    } else {
        slugs.join(", ")
    }
}

fn write_kind(output: &mut String, kind: AddKind) {
    writeln!(output, "{} — {}", kind.as_str(), kind.description())
        .expect("writing to a String cannot fail");

    for blueprint in BLUEPRINTS.iter().filter(|entry| entry.kind == kind) {
        let aliases = if blueprint.aliases.is_empty() {
            String::new()
        } else {
            format!(" (alias: {})", blueprint.aliases.join(", "))
        };
        writeln!(
            output,
            "  {:<27} {}",
            format!("{}{}", blueprint.slug, aliases),
            blueprint.description
        )
        .expect("writing to a String cannot fail");
    }

    let url_description = match kind {
        AddKind::Upstream => "any Anthropic- or OpenAI-compatible endpoint (research-driven)",
        AddKind::Provider => "research-driven adapter implementation guide",
    };
    writeln!(output, "  {:<27} {url_description}", "<url>")
        .expect("writing to a String cannot fail");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_slug_and_alias_resolves_to_its_body() {
        for blueprint in BLUEPRINTS {
            assert_eq!(
                resolve(blueprint.kind, blueprint.slug).unwrap(),
                blueprint.body
            );
            for alias in blueprint.aliases {
                assert_eq!(resolve(blueprint.kind, alias).unwrap(), blueprint.body);
            }
        }
    }

    #[test]
    fn upstream_slugs_and_aliases_are_rejected_as_provider_blueprints() {
        for blueprint in BLUEPRINTS
            .iter()
            .filter(|blueprint| blueprint.kind == AddKind::Upstream)
        {
            for name in std::iter::once(blueprint.slug).chain(blueprint.aliases.iter().copied()) {
                let error = resolve(AddKind::Provider, name).unwrap_err().to_string();
                assert!(
                    error.contains("unknown provider blueprint"),
                    "unexpected error for {name:?}: {error:?}"
                );
                assert!(
                    error.contains("none (URL only)"),
                    "missing provider availability for {name:?}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_name_lists_available_slugs_and_url_form() {
        let error = resolve(AddKind::Upstream, "nope").unwrap_err().to_string();
        for slug in [
            "anthropic",
            "codex",
            "openai",
            "xai",
            "grok",
            "kimi",
            "cursor",
        ] {
            assert!(error.contains(slug), "missing {slug:?} in {error:?}");
        }
        assert!(error.contains("absolute"));
        assert!(error.contains("http://"));
        assert!(error.contains("https://"));
    }

    #[test]
    fn accepts_absolute_http_urls_and_injects_canonical_serialization() {
        for (input, expected) in [
            ("HTTP://EXAMPLE.COM:80/docs", "http://example.com/docs"),
            (
                "HTTPS://EXAMPLE.COM:443/provider/../docs",
                "https://example.com/docs",
            ),
        ] {
            let body = resolve(AddKind::Upstream, input).unwrap();
            assert!(body.contains(expected), "missing {expected:?} in output");
            assert!(
                !body.contains(input),
                "raw input was interpolated: {input:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_and_non_absolute_urls() {
        for value in [
            "https://",
            "http:///path",
            "https:// bad",
            "https://example.com/line\nbreak",
            "https://user@example.com/docs",
            "https://user:secret@example.com/docs",
            "ftp://example.com/docs",
            "file:///tmp/docs",
            "./relative",
            "foo/bar",
            "//example.com",
            "?query",
            "#fragment",
        ] {
            for kind in [AddKind::Upstream, AddKind::Provider] {
                assert!(
                    resolve(kind, value).is_err(),
                    "accepted {value:?} as {kind:?}"
                );
            }
        }
    }

    #[test]
    fn non_http_urls_follow_the_unknown_blueprint_path() {
        for value in ["ftp://example.com/docs", "file:///tmp/docs"] {
            let error = resolve(AddKind::Provider, value).unwrap_err().to_string();
            assert!(
                error.contains("unknown provider blueprint"),
                "unexpected error for {value:?}: {error:?}"
            );
            assert!(!error.contains("invalid research URL"));
        }
    }

    #[test]
    fn url_shaped_inputs_report_specific_validation_errors() {
        for (value, reason) in [
            ("https://", "missing host"),
            ("http:///path", "missing host"),
            ("https:// bad", "whitespace is not allowed"),
            (
                "https://user@example.com/docs",
                "credentials are not allowed",
            ),
            (
                "https://user:secret@example.com/docs",
                "credentials are not allowed",
            ),
            ("https://example.com:bad", "invalid port number"),
        ] {
            let error = resolve(AddKind::Provider, value).unwrap_err().to_string();
            assert!(
                error.contains("invalid research URL"),
                "unexpected error for {value:?}: {error:?}"
            );
            assert!(
                error.contains(reason),
                "missing {reason:?} for {value:?}: {error:?}"
            );
            assert!(!error.contains("unknown provider blueprint"));
        }
    }

    #[test]
    fn research_url_uses_the_distinct_template_for_each_kind() {
        let url = "https://example.com/provider/docs";
        let upstream = resolve(AddKind::Upstream, url).unwrap();
        let provider = resolve(AddKind::Provider, url).unwrap();

        for body in [&upstream, &provider] {
            assert!(body.contains(url));
            assert!(!body.contains(RESEARCH_URL));
        }
        assert!(upstream.contains("## Add the upstream"));
        assert!(!upstream.contains("## Establish the protocol contract"));
        assert!(provider.contains("## Establish the protocol contract"));
        assert!(!provider.contains("## Add the upstream"));
        assert_ne!(upstream, provider);
    }

    #[test]
    fn every_registered_blueprint_has_a_markdown_heading() {
        for blueprint in BLUEPRINTS {
            assert!(
                !blueprint.body.trim().is_empty(),
                "{} is empty",
                blueprint.slug
            );
            assert!(
                blueprint.body.starts_with("# "),
                "{} has no heading",
                blueprint.slug
            );
        }
        for body in [GENERIC_UPSTREAM, GENERIC_PROVIDER] {
            assert!(!body.trim().is_empty());
            assert!(body.starts_with("# "));
        }
    }

    #[test]
    fn full_listing_contains_every_slug_and_both_kinds() {
        let output = list();
        assert!(output.contains("upstream —"));
        assert!(output.contains("provider —"));
        for blueprint in BLUEPRINTS {
            assert!(output.contains(blueprint.slug));
        }
    }

    #[test]
    fn full_listing_displays_registered_aliases_without_empty_suffixes() {
        let output = list();
        for blueprint in BLUEPRINTS {
            let line = output
                .lines()
                .find(|line| line.trim_start().starts_with(blueprint.slug))
                .unwrap_or_else(|| panic!("missing listing line for {:?}", blueprint.slug));
            if blueprint.aliases.is_empty() {
                assert!(
                    !line.contains("(alias:"),
                    "unexpected alias suffix for {:?}: {line:?}",
                    blueprint.slug
                );
            } else {
                let expected = format!(
                    "{} (alias: {})",
                    blueprint.slug,
                    blueprint.aliases.join(", ")
                );
                assert!(
                    line.trim_start().starts_with(&expected),
                    "missing alias display {expected:?} in {line:?}"
                );
            }
        }
    }

    #[test]
    fn kind_listing_is_scoped_and_includes_url_usage() {
        let upstreams = list_kind(AddKind::Upstream);
        assert!(upstreams.contains("kimi"));
        assert!(upstreams.contains("<url>"));
        assert!(!upstreams.contains("implement support for a new provider"));

        let providers = list_kind(AddKind::Provider);
        assert!(providers.contains("provider —"));
        assert!(providers.contains("https://example.com/api-docs"));
        assert!(!providers.contains("kimi"));
    }
}

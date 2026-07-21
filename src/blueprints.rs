use std::fmt::Write as _;

use anyhow::bail;
use clap::ValueEnum;

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

const BLUEPRINTS: &[Blueprint] = &[
    Blueprint {
        kind: AddKind::Upstream,
        slug: "anthropic",
        aliases: &["claude"],
        description: "Anthropic API — passthrough or pooled Claude OAuth accounts",
        body: include_str!("../blueprints/upstream/anthropic.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "codex",
        aliases: &["chatgpt"],
        description: "ChatGPT/Codex backend via chatgpt_oauth",
        body: include_str!("../blueprints/upstream/codex.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "openai",
        aliases: &[],
        description: "OpenAI Responses API via OPENAI_API_KEY",
        body: include_str!("../blueprints/upstream/openai.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "xai",
        aliases: &[],
        description: "xAI API via XAI_API_KEY",
        body: include_str!("../blueprints/upstream/xai.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "grok",
        aliases: &[],
        description: "SuperGrok subscription via xai_oauth login",
        body: include_str!("../blueprints/upstream/grok.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "kimi",
        aliases: &["moonshot"],
        description: "Moonshot Kimi (Anthropic-compatible) via MOONSHOT_API_KEY",
        body: include_str!("../blueprints/upstream/kimi.md"),
    },
    Blueprint {
        kind: AddKind::Upstream,
        slug: "cursor",
        aliases: &[],
        description: "Cursor subscription via cursor_oauth login",
        body: include_str!("../blueprints/upstream/cursor.md"),
    },
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
    if is_absolute_http_url(name_or_url) {
        return Ok(generic(kind).replace(RESEARCH_URL, name_or_url));
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

fn is_absolute_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
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
    fn resolves_slugs_and_aliases() {
        let kimi = resolve(AddKind::Upstream, "kimi").unwrap();
        assert_eq!(kimi, resolve(AddKind::Upstream, "moonshot").unwrap());
        assert_eq!(
            resolve(AddKind::Upstream, "anthropic").unwrap(),
            resolve(AddKind::Upstream, "claude").unwrap()
        );
        assert_eq!(
            resolve(AddKind::Upstream, "codex").unwrap(),
            resolve(AddKind::Upstream, "chatgpt").unwrap()
        );
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
    fn relative_paths_and_non_urls_are_rejected() {
        for value in ["./x", "foo/bar", "docs.example.com/provider"] {
            assert!(
                resolve(AddKind::Upstream, value).is_err(),
                "accepted {value}"
            );
            assert!(
                resolve(AddKind::Provider, value).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn research_url_is_fully_injected_for_both_kinds() {
        let url = "https://example.com/provider/docs";
        for kind in [AddKind::Upstream, AddKind::Provider] {
            let body = resolve(kind, url).unwrap();
            assert!(body.contains(url));
            assert!(!body.contains(RESEARCH_URL));
        }
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

use hive_md::{EntityMention, MentionKind, TypedKind, parse};

fn mention(parsed: &hive_md::ParsedBody, raw: &str) -> Option<EntityMention> {
    parsed
        .entity_mentions
        .iter()
        .find(|m| m.raw == raw)
        .cloned()
}

#[test]
fn extracts_person_mentions_from_prose() {
    let body = "Pinged @pia about it. cc @apis-prime here.";
    let parsed = parse(body);

    assert_eq!(parsed.entity_mentions.len(), 2);
    let pia = mention(&parsed, "@pia").expect("pia");
    assert!(matches!(pia.kind, MentionKind::Person));
    assert_eq!(pia.slug, "pia");
    assert_eq!(pia.line_index, 0);

    let apis = mention(&parsed, "@apis-prime").expect("apis-prime");
    assert!(matches!(apis.kind, MentionKind::Person));
    assert_eq!(apis.slug, "apis-prime");
}

#[test]
fn strips_trailing_punctuation_from_person() {
    let body = "Following up with @pia.";
    let parsed = parse(body);
    let m = mention(&parsed, "@pia").expect("pia");
    assert_eq!(m.slug, "pia");
}

#[test]
fn escapes_double_at_sign() {
    // `@@pia` should NOT be a mention. The first `@` is literal text.
    let body = "Literal text: @@pia is not a mention.";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

#[test]
fn extracts_typed_wikilink() {
    let body = "See [[task:ship-feature]] for details.";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "ship-feature");
    assert_eq!(m.raw, "[[task:ship-feature]]");
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
}

#[test]
fn tolerates_whitespace_inside_wikilink() {
    let body = "ref: [[ task : abc ]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "abc");
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
}

#[test]
fn extracts_fuzzy_wikilink() {
    let body = "See [[home-page]] for canonical setup.";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    assert_eq!(parsed.entity_mentions[0].slug, "home-page");
    assert!(matches!(parsed.entity_mentions[0].kind, MentionKind::Fuzzy));
}

#[test]
fn typed_kinds_cover_all_four() {
    let body = "
[[task:t1]] [[note:n1]] [[event:e1]] [[journal:j1]]
";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 4);
    let kinds: Vec<MentionKind> = parsed.entity_mentions.iter().map(|m| m.kind).collect();
    assert!(kinds.contains(&MentionKind::Typed(TypedKind::Task)));
    assert!(kinds.contains(&MentionKind::Typed(TypedKind::Note)));
    assert!(kinds.contains(&MentionKind::Typed(TypedKind::Event)));
    assert!(kinds.contains(&MentionKind::Typed(TypedKind::Journal)));
}

#[test]
fn invalid_typed_kind_is_dropped() {
    // `[[foo:bar]]` ... `foo` isn't a valid typed kind. Drop.
    let body = "[[foo:bar]]";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

#[test]
fn invalid_slug_is_dropped() {
    // Slug must start with `[a-z]`. Numeric / uppercase first char = drop.
    let body = "[[1abc]] and @PIA";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

#[test]
fn skips_inline_code_span() {
    // The shell command in backticks contains `@pia` and `[[task:foo]]` ... must
    // NOT show up. Outside the code span, `@apis` still does.
    let body = "Run `echo @pia and [[task:foo]]` then ping @apis.";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    assert_eq!(parsed.entity_mentions[0].slug, "apis");
}

#[test]
fn skips_fenced_code_block() {
    let body = "\
Before the block, @before applies.

```bash
ssh @nobody && cat [[task:hidden]]
```

After the block, @after applies.
";
    let parsed = parse(body);
    let slugs: Vec<_> = parsed
        .entity_mentions
        .iter()
        .map(|m| m.slug.as_str())
        .collect();
    assert!(slugs.contains(&"before"));
    assert!(slugs.contains(&"after"));
    assert!(!slugs.contains(&"nobody"));
    assert!(!slugs.contains(&"hidden"));
}

#[test]
fn task_lines_still_surface_their_mentions() {
    let body = "- [ ] follow up with @pia about [[note:dinner]] ^task1";
    let parsed = parse(body);
    // The task itself is parsed.
    assert_eq!(parsed.tasks.len(), 1);
    assert_eq!(parsed.tasks[0].owner.as_deref(), Some("pia"));

    // And `@pia` + `[[note:dinner]]` are also visible to the links projection.
    let slugs: Vec<_> = parsed
        .entity_mentions
        .iter()
        .map(|m| m.slug.as_str())
        .collect();
    assert!(slugs.contains(&"pia"));
    assert!(slugs.contains(&"dinner"));
}

#[test]
fn line_indices_are_correct() {
    let body = "line zero\nline one with @pia\nline two with [[task:x]]";
    let parsed = parse(body);

    let pia = mention(&parsed, "@pia").expect("pia");
    assert_eq!(pia.line_index, 1);
    let x = mention(&parsed, "[[task:x]]").expect("x");
    assert_eq!(x.line_index, 2);
}

#[test]
fn empty_wikilink_is_dropped() {
    let body = "[[]] and [[ ]] and [[:]]";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

#[test]
fn nested_brackets_do_not_parse() {
    // `[[a [[ b]]` ... the outer `[[a` doesn't have a closing `]]` BEFORE a
    // new opening, so we drop the outer. The inner `[[b]]` is still a valid
    // fuzzy mention.
    let body = "[[outer [[inner]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    assert_eq!(parsed.entity_mentions[0].slug, "inner");
}

#[test]
fn multiple_at_mentions_on_one_line() {
    let body = "@one @two @three";
    let parsed = parse(body);
    let slugs: Vec<_> = parsed
        .entity_mentions
        .iter()
        .map(|m| m.slug.as_str())
        .collect();
    assert_eq!(slugs, vec!["one", "two", "three"]);
}

// ----- title-aware fuzzy resolver (parser side) -----

#[test]
fn fuzzy_accepts_multi_word_title_and_slugifies() {
    // `[[Multi Word Title]]` ... the parser normalizes the inner via the
    // shared derive_slug rule (lowercase, non-alnum → '-', collapse, trim).
    let body = "See [[Multi Word Title]] for details.";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "multi-word-title");
    assert_eq!(m.raw, "[[Multi Word Title]]");
    assert!(matches!(m.kind, MentionKind::Fuzzy));
}

#[test]
fn fuzzy_title_with_punctuation_slugifies() {
    // Matches the load-bearing example from the task: a journal entry titled
    // "Fix the Traefik 60s issue" gets slug "fix-the-traefik-60s-issue" on
    // insert. The parser must emit the same slug from the title form.
    let body = "ref [[Fix the Traefik 60s issue]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    assert_eq!(parsed.entity_mentions[0].slug, "fix-the-traefik-60s-issue");
    assert!(matches!(parsed.entity_mentions[0].kind, MentionKind::Fuzzy));
}

#[test]
fn typed_link_still_resolves_with_literal_slug() {
    // `[[type:slug]]` unchanged: the typed prefix routes to the right table.
    let body = "see [[task:fix-traefik]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert_eq!(m.slug, "fix-traefik");
}

#[test]
fn typed_link_accepts_a_title_after_the_prefix() {
    // `[[task:Fix the Traefik]]` ... the typed prefix wins over the all-table
    // fuzzy, and the title gets slugified the same way as the fuzzy path.
    let body = "see [[task:Fix the Traefik]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert_eq!(m.slug, "fix-the-traefik");
}

#[test]
fn fuzzy_with_only_punctuation_inner_drops() {
    // After slugify the inner is empty ... no mention.
    let body = "[[!!!]] and [[ - - - ]]";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

#[test]
fn fuzzy_inner_starting_with_digit_drops() {
    // Slugs can't start with a digit. `[[2026 plan]]` slugifies to "2026-plan"
    // which violates the constraint, so we drop it rather than mint a slug
    // the schema would reject anyway.
    let body = "[[2026 plan]]";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

// ----- transparent anchor: alias + UUID identifier -----

#[test]
fn typed_uuid_identifier_with_alias_parses() {
    // The compose picker writes this shape: `[[type:<uuid>|<title>]]`. The
    // identifier resolves by UUID, and the alias is the human-readable
    // label captured at write time.
    let uuid_str = "019e745e-c480-7b1b-846c-9108b9af1b19";
    let body = format!("see [[task:{uuid_str}|Fix the build]] please");
    let parsed = parse(&body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert_eq!(m.target_id.expect("uuid parsed").to_string(), uuid_str);
    assert_eq!(m.alias.as_deref(), Some("Fix the build"));
    // Slug becomes a slugified alias (so a UUID-gone-stale can still try slug).
    assert_eq!(m.slug, "fix-the-build");
}

#[test]
fn typed_slug_with_alias_parses() {
    // Hand-typed mention with alias: identifier is still a slug; alias just
    // overrides the display text.
    let body = "see [[task:ship-feature|Ship the feature]] today";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert!(m.target_id.is_none());
    assert_eq!(m.slug, "ship-feature");
    assert_eq!(m.alias.as_deref(), Some("Ship the feature"));
}

#[test]
fn fuzzy_uuid_identifier_with_alias_parses() {
    let uuid_str = "019e745e-c480-7b1b-846c-9108b9af1b19";
    let body = format!("recall [[{uuid_str}|Traefik outage]] from last week");
    let parsed = parse(&body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Fuzzy));
    assert_eq!(m.target_id.expect("uuid parsed").to_string(), uuid_str);
    assert_eq!(m.alias.as_deref(), Some("Traefik outage"));
}

#[test]
fn fuzzy_title_with_alias_parses() {
    // Freeform title plus a display alias.
    let body = "see [[some-slug|Display Label]] for more";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Fuzzy));
    assert!(m.target_id.is_none());
    assert_eq!(m.slug, "some-slug");
    assert_eq!(m.alias.as_deref(), Some("Display Label"));
}

#[test]
fn alias_is_optional_typed() {
    // Existing `[[type:slug]]` shape (no `|`) keeps alias = None.
    let body = "see [[task:ship-feature]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert_eq!(m.slug, "ship-feature");
    assert!(m.alias.is_none());
}

#[test]
fn alias_is_optional_fuzzy() {
    let body = "see [[home-page]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Fuzzy));
    assert_eq!(m.slug, "home-page");
    assert!(m.alias.is_none());
}

#[test]
fn empty_alias_is_treated_as_no_alias() {
    // `[[task:slug|]]` ... pipe with empty alias. Slug still resolves; alias
    // is None (the trim makes the empty payload disappear).
    let body = "see [[task:ship-feature|]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "ship-feature");
    assert!(m.alias.is_none());
}

#[test]
fn second_pipe_is_part_of_alias() {
    // Only the FIRST `|` is the alias-separator. A second pipe inside the
    // alias is treated as literal text in the display string.
    let body = "see [[task:slug|alias|with pipe]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "slug");
    assert_eq!(m.alias.as_deref(), Some("alias|with pipe"));
}

#[test]
fn alias_tolerates_whitespace_around_pipe() {
    let body = "see [[task:ship-feature  |  Ship the feature  ]]";
    let parsed = parse(body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert_eq!(m.slug, "ship-feature");
    assert_eq!(m.alias.as_deref(), Some("Ship the feature"));
}

#[test]
fn typed_uuid_with_no_alias_falls_back_to_kind_slug() {
    // `[[task:<uuid>]]` with no alias ... target_id is set, but the slug
    // fallback is just the kind name (it's a sentinel, the resolver uses
    // target_id). Stable, never matches a real row.
    let uuid_str = "019e745e-c480-7b1b-846c-9108b9af1b19";
    let body = format!("see [[task:{uuid_str}]]");
    let parsed = parse(&body);
    assert_eq!(parsed.entity_mentions.len(), 1);
    let m = &parsed.entity_mentions[0];
    assert!(matches!(m.kind, MentionKind::Typed(TypedKind::Task)));
    assert_eq!(m.target_id.expect("uuid parsed").to_string(), uuid_str);
    assert!(m.alias.is_none());
    assert_eq!(m.slug, "task");
}

#[test]
fn alias_only_pipe_with_empty_head_drops() {
    // `[[|alias]]` ... empty identifier; not a valid mention.
    let body = "see [[|alias only]] please";
    let parsed = parse(body);
    assert!(parsed.entity_mentions.is_empty());
}

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

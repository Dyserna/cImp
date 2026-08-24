//! The pure address and URL predicates behind the outbound screen.
//!
//! Two questions, no state: **is this address inside a range an EXTERNAL fetch
//! may never reach** ([`is_denied_ip`]), and **which URL-shaped runs does this
//! argument blob contain** ([`extract_urls`] and the scanners under it). Neither
//! reads settings, a [`Policy`](super::Policy), a [`Budget`](super::Budget) or a
//! [`Flag`](super::Flag) — which is why they are here and the screen that
//! consumes them is next door.
//!
//! V42 R19 (#126) lifted this out of `outbound.rs` verbatim. The file is
//! SECURITY-CRITICAL for the same reason it always was: every one of these
//! predicates is a step of the SSRF denial, so a "simplification" here widens
//! the hole. The two properties that pin the whole set —
//! [`tests::is_denied_ip_covers_every_locked_range_and_no_more`] and the
//! extractor’s widened-and-ordered corpus — moved with the code they pin.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde_json::Value;
use url::{Host, Url};

// ── The IP range policy ────────────────────────────────────────────────────

/// Whether `ip` is inside one of the ranges an EXTERNAL fetch may never reach.
///
/// The set is the locked decision-11 list, and it is deliberately expressed as
/// octet math over `std::net` types rather than a CIDR dependency: ten fixed
/// ranges are less code than a crate, and a security predicate everyone can
/// read in one screen is worth more than a general parser.
///
/// - `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` — RFC1918 private.
/// - `127.0.0.0/8`, `::1` — loopback. cImp's own services live here.
/// - `169.254.0.0/16`, `fe80::/10` — link-local, i.e. the cloud metadata
///   endpoint `169.254.169.254`.
/// - `100.64.0.0/10` — CGNAT, routable-looking but inside the carrier's net.
/// - `0.0.0.0/8` and `::` — "this network"; `0.0.0.0` reaches localhost on
///   Linux.
/// - `::ffff:0:0/96` — IPv4-mapped IPv6, unmapped and re-checked, else
///   `::ffff:192.168.1.1` slips a private v4 past a v4-only screen.
///
/// Four additions **beyond** the spec's enumeration, all closing exactly the
/// hole the mapped-IPv6 entry exists to close — a v6 spelling of a v4 address
/// the v4 screen already denies:
/// - `fc00::/7` — IPv6 unique-local, the v6 analogue of RFC1918. Omitting it
///   would leave the v6 private range wide open while the v4 one is closed.
/// - IPv4-compatible IPv6 (`::a.b.c.d`, deprecated) — same unmap-and-recheck,
///   so `::7f00:1` cannot spell loopback past the v4 screen.
/// - `64:ff9b::/96` — the well-known NAT64 prefix (RFC 6052), the one range the
///   V32 review found missing (#48).
/// - `2002::/16` — 6to4 (RFC 3056), where `2002:7f00:1::` spells 127.0.0.1.
///
/// The last two are **unmapped and re-checked**, not blanket-denied, for the
/// same reason `::ffff:` is: both prefixes embed a *destination* v4 address, so
/// the address that matters is the embedded one. `64:ff9b::8.8.8.8` is a public
/// destination reached through a translator and stays allowed;
/// `64:ff9b::7f00:1` is loopback wearing a v6 hat and does not. Blanket-denying
/// either prefix would deny more than the policy says and buy nothing — an
/// embedded *public* v4 is not an internal service.
///
/// Teredo (`2001::/32`) is deliberately **not** here: its embedded v4s are the
/// relay server and the obfuscated *client*, not the destination the packet
/// reaches, so unmapping it would screen the wrong address.
pub fn is_denied_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_denied_v4(v4),
        IpAddr::V6(v6) => is_denied_v6(v6),
    }
}

fn is_denied_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    match a {
        // 0.0.0.0/8 — "this network" (and 0.0.0.0 == localhost on Linux).
        0 => true,
        // 10.0.0.0/8 — RFC1918.
        10 => true,
        // 127.0.0.0/8 — loopback.
        127 => true,
        // 100.64.0.0/10 — CGNAT.
        100 => (64..=127).contains(&b),
        // 169.254.0.0/16 — link-local (cloud metadata).
        169 => b == 254,
        // 172.16.0.0/12 — RFC1918.
        172 => (16..=31).contains(&b),
        // 192.168.0.0/16 — RFC1918.
        192 => b == 168,
        _ => false,
    }
}

fn is_denied_v6(ip: Ipv6Addr) -> bool {
    // ::1 and :: first: both are also "IPv4-compatible" by the bit pattern
    // below, and mapping them to 0.0.0.1 / 0.0.0.0 would be misleading (they
    // are denied here on their own terms).
    if ip.is_loopback() || ip == Ipv6Addr::UNSPECIFIED {
        return true;
    }
    // ::ffff:a.b.c.d — the mapped form the spec calls out by name.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_denied_v4(v4);
    }
    let segs = ip.segments();
    let o = ip.octets();
    // ::a.b.c.d — the deprecated IPv4-compatible form. Still accepted by some
    // stacks, so unmap and re-check rather than letting `::7f00:1` through.
    if segs[..6].iter().all(|s| *s == 0) {
        return is_denied_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 64:ff9b::/96 — the well-known NAT64 prefix (RFC 6052). The v4 destination
    // is the low 32 bits, so this is the same unmap-and-recheck as `::ffff:`
    // (#48). A translator forwards to whatever is embedded here; if that is
    // 127.0.0.1 or 10.x, the fetch reaches the internal service by another
    // spelling.
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6].iter().all(|s| *s == 0) {
        return is_denied_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 2002::/16 — 6to4 (RFC 3056). The v4 endpoint sits in bytes 2..6, which is
    // how `2002:7f00:1::` spells 127.0.0.1. Reaching it needs a 6to4 relay, so
    // it is theoretical rather than practical — but it is three lines of the
    // machinery already here, and leaving one embedded-v4 form unscreened while
    // screening three others is the inconsistency that produces the next
    // finding.
    if segs[0] == 0x2002 {
        return is_denied_v4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    // fe80::/10 — link-local.
    if o[0] == 0xfe && (o[1] & 0xc0) == 0x80 {
        return true;
    }
    // fc00::/7 — unique-local (the v6 RFC1918). Beyond the spec's list; see the
    // function docs.
    if (o[0] & 0xfe) == 0xfc {
        return true;
    }
    false
}

// ── URL extraction ─────────────────────────────────────────────────────────

/// Scheme prefixes we treat as fetchable, **without** the slashes. Only these
/// two: a `file://` or `data:` argument is not an SSRF vector for a remote fetch
/// server, and widening the net here would only add false positives.
///
/// The slashes are not part of the constant since #48 finding H-3. WHATWG's
/// *special authority ignore slashes* state skips **any number** of `/` or `\`
/// after a special scheme's colon — including **zero** — so `http:/127.0.0.1`,
/// `http:127.0.0.1` and `http:\\127.0.0.1` all resolve to host `127.0.0.1`,
/// while a literal `"http://"` substring match saw none of them. Matching the
/// scheme and consuming the slash run separately ([`scan_scheme_runs`]) is what
/// makes the extractor agree with the parser instead of with one spelling.
pub(super) const URL_SCHEMES: [&str; 2] = ["http:", "https:"];

/// What a scheme-bearing run normalizes to once its slash run is consumed:
/// exactly the two slashes [`Url::parse`] wants, whatever was written.
pub(super) const NORMALIZED_SLASHES: &str = "//";

/// Characters that terminate a URL embedded in prose. Whitespace plus the
/// quoting/bracketing characters a model or a page would wrap a URL in.
///
/// Used by [`scan_bare_authorities`], where a run has no scheme and `\` is a
/// path separator in the Windows paths that scan walks over. A run that *does*
/// carry a scheme uses [`SCHEME_RUN_TERMINATORS`] instead — see there.
const URL_TERMINATORS: [char; 8] = ['"', '\'', '`', '<', '>', '\\', '|', '^'];

/// [`URL_TERMINATORS`] minus `\` — the terminator set for a run that already
/// carries an `http:`/`https:` scheme (#48, finding H-3).
///
/// For a **special** scheme WHATWG treats `\` as a slash everywhere: in the
/// authority-slash run, in the path, in place of `/`. So `http://\10.0.0.1`
/// has host `10.0.0.1` and `http://127.0.0.1\props` has path `/props`. Cutting
/// the run at the backslash handed the range check a string the fetcher never
/// sees; the parser does not stop there, so neither may we.
///
/// The remaining members are safe to cut at because a parser either **removes**
/// them (never — none of these is TAB/LF/CR, which [`WHATWG_STRIPPED`] handles)
/// or **cannot build an IP host past them**: `<`, `>`, `|` and `^` are
/// forbidden host code points and fail the parse outright, and `"`, `'` and
/// backtick, while not forbidden, are not digits — an authority containing one
/// is never an IPv4 or bracketed IPv6 literal, so it can only be a domain, and
/// a domain is screened by resolution, which such a name has no answer for.
/// The corpus test in this module's tests is what actually holds this claim
/// down: it feeds every one of these characters through [`Url::parse`] as the
/// oracle rather than trusting this paragraph.
pub(super) const SCHEME_RUN_TERMINATORS: [char; 7] = ['"', '\'', '`', '<', '>', '|', '^'];

/// The three characters a WHATWG-conformant URL parser **removes** from
/// anywhere in a URL before parsing (the spec's *ASCII tab or newline*): TAB,
/// LF, CR (#48, review finding C-4).
///
/// This is the whole parser differential. `scan_extraction` cuts a candidate at
/// the first whitespace, which includes these three — but the `url` crate cImp
/// itself uses loops `if !ascii_tab_or_new_line(c)` in `Input::next_utf8`, and
/// Node and Python's `urllib` do the same. So
/// `Url::parse("http://\t127.0.0.1:12344/props")` yields host `127.0.0.1`:
/// cImp's own screen would have caught the bypass had the extractor not cut the
/// string first. Every string is therefore scanned **twice** — once as written,
/// once with these three removed — and the union is screened. Scanning both
/// matters: stripping alone glues `…/a\nhttp://10.0.0.1/` into one run whose
/// host is the *first* URL, which the as-written scan is what separates.
pub(super) const WHATWG_STRIPPED: [char; 3] = ['\t', '\n', '\r'];

/// One URL-shaped run found in an argument, with the evidence that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    /// The run, normalized to something [`Url::parse`] can accept — a
    /// schemeless or protocol-relative run gets an assumed `http://`, which is
    /// what a scheme-guessing fetcher would do with it.
    pub(super) url: String,
    /// Whether a literal `http://`/`https://` prefix was present.
    ///
    /// Load-bearing for the deny-on-unparseable rule: an explicit scheme is
    /// unambiguous evidence that this run *is* a URL, so failing to understand
    /// it is not evidence of safety. A bare `host:port` run is a heuristic
    /// guess, and denying on a guess we cannot even parse would refuse prose.
    pub(super) strict: bool,
}

/// Every http/https URL appearing in **any** string value of `args`, at any
/// nesting depth.
///
/// Deliberately not "read the `url` parameter": the shape of an EXTERNAL
/// server's arguments is that server's business, and by the Phase A
/// unknown-⇒-EXTERNAL invariant we screen servers that do not exist yet. A
/// future server may name the field `target`, `endpoint`, `href`, or bury it in
/// an array of request objects. Scanning every string is the only version of
/// this that stays correct as servers are added.
///
/// Object *keys* are not scanned: a key is part of the server's schema, not
/// model-authored content.
pub fn extract_urls(args: &Value) -> Vec<String> {
    candidates(args).into_iter().map(|c| c.url).collect()
}

/// [`extract_urls`] with the provenance the screen needs. Order-preserving and
/// deduplicated: the two scan variants overlap by construction, and a duplicate
/// candidate is a duplicate DNS resolution for no new information.
pub(super) fn candidates(args: &Value) -> Vec<Candidate> {
    let mut out = Vec::new();
    collect_candidates(args, &mut out);
    out
}

fn collect_candidates(v: &Value, out: &mut Vec<Candidate>) {
    match v {
        Value::String(s) => scan_string(s, out),
        Value::Array(items) => items.iter().for_each(|i| collect_candidates(i, out)),
        Value::Object(map) => map.values().for_each(|i| collect_candidates(i, out)),
        _ => {}
    }
}

/// Record one candidate, merging with an existing identical run rather than
/// repeating it. `strict` is OR-ed: if any scan saw an explicit scheme, the run
/// is strict.
fn push_candidate(url: String, strict: bool, out: &mut Vec<Candidate>) {
    match out.iter_mut().find(|c| c.url == url) {
        Some(existing) => existing.strict |= strict,
        None => out.push(Candidate { url, strict }),
    }
}

/// Pull every URL-looking run out of one string. A single argument can carry
/// several (a search query listing sources, a prompt quoting a page).
///
/// Scanned as written **and**, when it contains any of [`WHATWG_STRIPPED`],
/// again with those removed — see that constant. A string with none of the
/// three (the overwhelming majority) is scanned exactly once and behaves
/// identically to the pre-#48 extractor, which keeps the new behaviour confined
/// to precisely the strings that trigger the differential.
fn scan_string(s: &str, out: &mut Vec<Candidate>) {
    scan_variant(s, out);
    if s.contains(WHATWG_STRIPPED) {
        let stripped: String = s.chars().filter(|c| !WHATWG_STRIPPED.contains(c)).collect();
        scan_variant(&stripped, out);
    }
}

fn scan_variant(s: &str, out: &mut Vec<Candidate>) {
    scan_scheme_runs(s, out);
    scan_bare_authorities(s, out);
}

/// Runs that begin with an `http:` / `https:` scheme, in **any** of the
/// spellings a WHATWG parser accepts (#48, finding H-3).
///
/// The scheme is matched case-insensitively (`HTTP:` parses identically — the
/// parser lowercases it), then the *slash run* — zero or more `/` or `\` in any
/// mix — is consumed and re-emitted as exactly [`NORMALIZED_SLASHES`]. That is
/// the whole of WHATWG's *special authority slashes* + *special authority
/// ignore slashes* states, and it is what makes `http:127.0.0.1:12344/props`,
/// `http:/127.0.0.1:12344/props` and `http:\\127.0.0.1\props` all arrive at the
/// range check as the one thing they actually are.
pub(super) fn scan_scheme_runs(s: &str, out: &mut Vec<Candidate>) {
    let lower = s.to_ascii_lowercase();
    let mut from = 0usize;
    while from < lower.len() {
        let Some((start, scheme)) = URL_SCHEMES
            .iter()
            .filter_map(|p| lower[from..].find(p).map(|i| (from + i, *p)))
            .min_by_key(|(i, _)| *i)
        else {
            return;
        };
        // Past the colon, then past the slash run — of any length, including
        // none, and `\` counts. `http:` with nothing after it lands `body_start`
        // at the end of the string, which is the scheme-only case.
        let after_colon = start + scheme.len();
        let body_start = after_colon
            + s[after_colon..]
                .find(|c: char| c != '/' && c != '\\')
                .unwrap_or(s.len() - after_colon);
        let end = s[body_start..]
            .find(|c: char| c.is_whitespace() || SCHEME_RUN_TERMINATORS.contains(&c))
            .map_or(s.len(), |i| body_start + i);
        let body = trim_trailing_punctuation(&s[body_start..end]);
        // The scheme is emitted **lowercased** — `scheme` is the matched
        // constant, not `s[start..after_colon]`. `Url::parse` lowercases it
        // anyway, the slash run beside it is already normalized (so preserving
        // only the case would be half-fidelity), and canonicalizing here is
        // what keeps `HTTP:\10.0.0.1:8080/x` from producing two identical
        // candidates — one from here, one from `scan_bare_authorities` across
        // the backslash — and therefore two resolutions of one target.
        push_candidate(format!("{scheme}{NORMALIZED_SLASHES}{body}"), true, out);
        from = end.max(start + 1);
    }
}

/// Strip the sentence punctuation a URL picked up from the prose around it —
/// but keep a closing bracket that **balances** one inside the run.
///
/// `,`, `.` and `;` are never part of a URL at the end. A closing bracket is,
/// when it closes something: `http://[::1]` is a bracketed IPv6 authority and
/// `https://en.example.org/Foo_(bar)` is an ordinary path. The unbalanced case
/// is the markdown one — `[label](http://host/x)` and `<http://host/x>` — where
/// the bracket belongs to the prose.
///
/// Found by the generated corpus (#48, H-3): the flat `trim_end_matches` this
/// replaces ate the `]` of every bracketed IPv6 URL that ended at its
/// authority, so `http://[2001:db8::1]` became the unparseable
/// `http://[2001:db8::1` — and the deny-on-unparseable rule then refused a
/// public target. A false refusal rather than a bypass, but the same
/// screen/parser disagreement H-3 is about, pointing the other way. The same
/// reasoning is already written into [`scan_bare_authorities`], which is why
/// *its* trim set never contained `]`.
fn trim_trailing_punctuation(run: &str) -> &str {
    let mut end = run.len();
    while let Some(c) = run[..end].chars().next_back() {
        let drop = match c {
            ',' | '.' | ';' => true,
            ')' | ']' | '}' => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                run[..end].matches(open).count() < run[..end].matches(c).count()
            }
            _ => false,
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &run[..end]
}

/// Runs with **no** scheme that a scheme-guessing client would still fetch:
/// a protocol-relative `//169.254.169.254/latest`, and a bare `127.0.0.1:8080/`
/// (#48). Neither produces a single candidate under [`scan_scheme_runs`], so
/// before this they were screened by nothing at all while `curl`-shaped fetchers
/// resolve them happily.
///
/// The plausibility rules in [`is_plausible_authority`] are what keeps this from
/// refusing prose: a bare run needs an explicit port **or** a path, and a
/// numeric host must be one the app's own URL parser reads as an IPv4 literal
/// with a non-zero first octet ([`is_prose_shaped_ipv4`]). Without the first
/// rule, `"upgraded to 10.0.0.1"` is a refusal served for a build number;
/// without the second, `"the meeting at 12:30"` becomes `http://12:30/`, whose
/// WHATWG host is `0.0.0.12` — inside `0.0.0.0/8`, and therefore a refusal
/// served for a sentence about a meeting.
///
/// The residual those rules leave, deliberately: a bare IP with neither port
/// nor path (`{"url": "10.0.0.1"}`, which `curl` would fetch) is not extracted.
/// Extracting it means refusing every argument that so much as *mentions* a
/// private address — "what is 192.168.1.1" is an ordinary research question —
/// and a fetch argument that terse, with no port and no path, is the rarest
/// form of the rarest case. Recorded in the milestone's accepted residuals.
///
/// #48 M-18 adds a second, narrower residual on the same reasoning, with its own
/// accepted cost written down: see [`is_canonical_network_address`].
pub(super) fn scan_bare_authorities(s: &str, out: &mut Vec<Candidate>) {
    for word in s.split(|c: char| c.is_whitespace() || URL_TERMINATORS.contains(&c)) {
        // Square brackets are NOT trimmed here, unlike the scheme scan's
        // trailing-punctuation trim: `[::1]:9000/x` is a bracketed IPv6
        // authority, and eating either bracket turns the one v6 form of this
        // bypass into an unparseable run that the loose scan then discards.
        let word = word
            .trim_start_matches(['(', '{'])
            .trim_end_matches([',', '.', ';', ')', '}']);
        // A run with a scheme is `scan_scheme_runs`'s business; a run with some
        // *other* scheme (`mailto:`, `file:`) is not a fetch we screen.
        if word.is_empty() || word.contains("://") {
            continue;
        }
        // Protocol-relative runs are explicit URL syntax, so nothing more is
        // required of them. A bare run must carry a port or a path — otherwise
        // every dotted word in prose becomes a DNS lookup, and every four-part
        // build number (`10.0.0.1`) becomes a refusal.
        let (rest, relative) = match word.strip_prefix("//") {
            Some(rest) => (rest, true),
            None => (word, false),
        };
        // #48 M-18: CIDR notation in prose is a range, not a target. The guard
        // fires ONLY here and only on a run that is neither scheme-bearing
        // (`strict` is `false` for everything this function emits) nor
        // protocol-relative — see [`is_canonical_network_address`] for the
        // conditions and for the cost that was accepted to get this.
        if !relative && is_canonical_network_address(rest) {
            continue;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let has_path = authority.len() < rest.len();
        if !is_plausible_authority(authority, !relative && !has_path) {
            continue;
        }
        push_candidate(format!("http://{rest}"), false, out);
    }
}

/// Whether a **schemeless** run is CIDR notation naming a canonical network
/// address — `10.0.0.0/8` — rather than anything a fetcher could reach (#48,
/// review finding M-18; user decision 2026-08-11, recorded as the review
/// amendment under locked decision 11).
///
/// # Why an exemption exists at all
///
/// The widened extraction reads `host/path` as a URL, and `/8` is a path. So
/// `"RFC1918 (10.0.0.0/8)"` — in a **search query**, in a doc placeholder, in a
/// sentence about which ranges are private — produced the candidate
/// `http://10.0.0.0/8`, whose host is inside `10/8`, and the whole call was
/// refused with a security error. Refusing ordinary research prose is the
/// failure mode that gets a security control switched off (locked decision 16's
/// argument), and it **compounds**: every benign denial advances the
/// power-of-two threshold in [`AuditClaims::claim_ssrf`] that would otherwise
/// have surfaced a *real* denial later, so noise suppresses signal.
///
/// # The conditions, all five
///
/// A run is exempt only when it is a network address in the one spelling a
/// network address has: host bits all zero for the stated prefix, a literal
/// `/`, a prefix of `0..=31`, **no port**, and **no further path segment** past
/// the prefix. `10.0.0.1/8` (host bits set), `10.0.0.1/32` and `10.0.0.0/32`
/// (`/32` is a single host, not a network), `10.0.0.0:80/8` (a port is a
/// service) and `10.0.0.0/8/admin` (a path is a fetch) all still deny.
/// Canonical spelling is required on both halves: [`Ipv4Addr`]'s parser rejects
/// leading zeros, so `010.0.0.0/8` — which WHATWG reads as *octal* — is not
/// exempt, and the prefix must be plain digits without a leading zero.
///
/// # The accepted cost, stated
///
/// A scheme-guessing fetcher could now reach `http://127.0.0.0/8` on port 80.
/// Accepted with eyes open: that string is a network **address** — host bits
/// zero, portless, pathless past the prefix — and therefore not the loopback
/// *service* locked decision 11's carve-out text protects. Naming a whole /8 is
/// not how anyone reaches a listener. The deny set itself is untouched: nothing
/// leaves it, and the *extraction* merely declines to manufacture this one class
/// of candidate.
///
/// # H-3's invariant is untouched
///
/// H-3 is "the screen and the parser must agree", and this guard cannot reopen
/// it: it runs only on runs that are **schemeless and not protocol-relative**,
/// i.e. exactly the runs `Url::parse` itself cannot resolve (`10.0.0.0` is not a
/// scheme). No generated corpus case and no C-4 row meets the conditions above:
/// they are scheme-bearing, protocol-relative, port-bearing, or carry a real path
/// where a prefix would have to be (`10.0.0.1/admin`) — and
/// `the_whatwg_parser_differential_is_closed` still refuses all 29 of them. And
/// [`screen_urls`] denies on **any** candidate,
/// so a dropped candidate can never flip a verdict another candidate in the same
/// call already condemns. `the_h3_corpus_audits_the_cidr_exemption` runs the
/// corpus generator with a `/8` tail so H-3 asserts this rather than this
/// comment claiming it.
pub(super) fn is_canonical_network_address(run: &str) -> bool {
    let Some((host, prefix)) = run.split_once('/') else {
        return false;
    };
    // Nothing may follow the prefix: a second path segment, a query or a
    // fragment all mean the run addresses something inside a host.
    if prefix.is_empty() || prefix.contains(['/', '?', '#']) {
        return false;
    }
    // A port is a service, which is what this screen is for.
    if host.contains(':') {
        return false;
    }
    // One or two plain digits, no leading zero: `/08` is not how a prefix
    // length is written, and the narrower the exemption the smaller its cost.
    if !matches!(prefix.len(), 1 | 2)
        || !prefix.bytes().all(|b| b.is_ascii_digit())
        || (prefix.len() == 2 && prefix.starts_with('0'))
    {
        return false;
    }
    let Ok(addr) = host.parse::<Ipv4Addr>() else {
        return false;
    };
    let Ok(bits) = prefix.parse::<u32>() else {
        return false;
    };
    // `/32` names one HOST, so it is a target and not a range; `0..=31` always
    // leaves at least one host bit that must be zero.
    if bits > 31 {
        return false;
    }
    u32::from(addr) & (u32::MAX >> bits) == 0
}

/// Whether `auth` reads as a `host[:port]` a URL parser would accept, strictly
/// enough that ordinary prose does not qualify. See [`scan_bare_authorities`].
fn is_plausible_authority(auth: &str, port_required: bool) -> bool {
    if auth.is_empty() {
        return false;
    }
    if let Some(after_open) = auth.strip_prefix('[') {
        // `[v6]` / `[v6]:port` — unambiguous, so the only question is whether
        // the literal is real.
        let Some(close) = after_open.find(']') else {
            return false;
        };
        if after_open[..close].parse::<Ipv6Addr>().is_err() {
            return false;
        }
        let tail = &after_open[close + 1..];
        return match tail.strip_prefix(':') {
            Some(port) => is_port(port),
            None => tail.is_empty() && !port_required,
        };
    }
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (auth, None),
    };
    match port {
        Some(p) if !is_port(p) => return false,
        None if port_required => return false,
        _ => {}
    }
    is_plausible_host(host)
}

fn is_port(p: &str) -> bool {
    !p.is_empty() && p.len() <= 5 && p.parse::<u32>().is_ok_and(|n| n <= 65535)
}

/// A four-octet IPv4 literal in the canonical spelling, **anything else the
/// app's own URL parser reads as an IPv4 literal** ([`whatwg_ipv4_literal`],
/// minus the one prose carve-out in [`is_prose_shaped_ipv4`]), or a dotted name
/// whose last label starts with a letter.
///
/// The last clause is what keeps `router.lan` and `internal.example.com` while
/// rejecting `2026.08.07` and the rest of the numeric runs prose is full of that
/// are not addresses at all.
pub(super) fn is_plausible_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    // The canonical dotted quad, cheaply: `Ipv4Addr` and WHATWG cannot disagree
    // about four unpadded decimal octets, so this is a fast path and not a
    // second opinion.
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    if let Some(addr) = whatwg_ipv4_literal(host) {
        return !is_prose_shaped_ipv4(host, addr);
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    labels[labels.len() - 1].starts_with(|c: char| c.is_ascii_alphabetic())
        && labels
            .iter()
            .all(|l| l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Whether the app's own URL parser reads `host` as an IPv4 literal, and which
/// address it reads it as (#48, review findings F-33 and **F-36**).
///
/// # The disagreement this closes
///
/// [`Ipv4Addr`]'s parser accepts exactly one spelling: four unpadded decimal
/// octets. A WHATWG parser accepts a whole family beyond it, and every member
/// of that family was a run `is_plausible_host` rejected — so it produced **no
/// candidate at all**, the screen never ran, and a scheme-guessing fetcher
/// resolved it happily:
///
/// | written | `Url::parse` host |
/// |---|---|
/// | `0177.0.0.1` (F-33: octal) | `127.0.0.1` |
/// | `127.1`, `127.0.1` (**short forms**) | `127.0.0.1` |
/// | `10.1`, `192.168.1`, `172.16.1` | `10.0.0.1`, `192.168.0.1`, `172.16.0.1` |
/// | `169.254.43518` | `169.254.169.254` — the cloud metadata service |
/// | `127.0.0.1.` (trailing dot) | `127.0.0.1` |
/// | `0x7f.0.0.1`, `0177.0.0.0x1` (hex, mixed) | `127.0.0.1` |
/// | `2130706433`, `0x7f000001`, `017700000001` (dword) | `127.0.0.1` |
///
/// F-33 closed the first row with a hand-written predicate for that one shape.
/// F-36 measured the rest still open — none of them carries a leading zero, so
/// none of them was covered. Enumerating shapes is what C-4 was "fixed" three
/// times by; the fix is to stop enumerating.
///
/// # The parser is the only authority
///
/// So this asks [`Url::parse`] — the same code the fetch path uses and the same
/// code [`screen_one`] will use on the candidate — instead of re-deriving what a
/// host means. Nothing is re-spelled: the candidate is still emitted **verbatim**
/// as `http://{run}` and parsed again downstream. A second implementation of the
/// WHATWG IPv4 parser for the screen to disagree with is the *defect* (H-3's
/// whole family), not the fix, and one existed here until F-36 deleted it.
///
/// # The cheap gate in front of it
///
/// `is_plausible_host` runs on every word of every argument, so a `Url::parse`
/// per word would be a real cost on the hot path. An IPv4 literal in any WHATWG
/// spelling starts with an ASCII digit and is built only from hex digits, `x`
/// (the `0x` marker) and `.`, so anything else is turned away before the parse.
/// The gate can only ever *reject*; it never says yes on the parser's behalf.
pub(super) fn whatwg_ipv4_literal(host: &str) -> Option<Ipv4Addr> {
    if !host.starts_with(|c: char| c.is_ascii_digit())
        || !host
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b'x' || b == b'X')
    {
        return None;
    }
    match Url::parse(&format!("http://{host}/")).ok()?.host()? {
        Host::Ipv4(v4) => Some(v4),
        _ => None,
    }
}

/// The one carve-out from [`whatwg_ipv4_literal`]: a **short** IPv4 spelling —
/// fewer than four parts — whose first octet is zero (#48, finding F-36; user
/// decision 2026-08-12).
///
/// # Why a carve-out exists at all
///
/// Reading every IPv4 literal as plausible is what closes F-36, and it is also
/// what turns the numeric runs ordinary prose is made of into candidates —
/// because WHATWG pads a short form on the *left*, so a small number becomes an
/// address in `0.0.0.0/8`, which is denied:
///
/// | prose | host | reads as |
/// |---|---|---|
/// | `"mix them 0.5:1 by volume"` | `0.5` | `0.0.0.5` |
/// | `"the meeting is at 12:30"` | `12` | `0.0.0.12` |
/// | `"a 1/2 cup"`, `"open 24/7"` | `1`, `24` | `0.0.0.1`, `0.0.0.24` |
/// | `"on 10/11/2026"` | `10` | `0.0.0.10` |
///
/// [`screen_urls`] denies on **any** candidate, so one of these in a paragraph
/// refuses the whole call — and every benign refusal advances the power-of-two
/// threshold in [`AuditClaims::claim_ssrf`] that would otherwise have surfaced a
/// real one, so the noise suppresses the signal. Locked decision 16's argument:
/// a screen that refuses prose is a screen the user switches off.
///
/// # Why it costs no containment worth having
///
/// A first octet of zero is exactly what left-padding a *small* number produces,
/// and **not one** of F-36's bypass spellings has one — they all name a real
/// first octet (`127`, `10`, `192`, `172`, `169`). The carve-out is confined to
/// **short** forms for the same reason: `0.0.0.0` is a genuine SSRF target (it
/// routes to localhost on Linux), so the full four-part spelling of `0.0.0.0/8`
/// stays denied. Dropping `0.0.0.0/8` from the deny set instead would have been
/// cheaper and was rejected: it trades a real target for prose comfort.
///
/// # The residual, stated — and CLOSED 2026-08-12
///
/// A *short* spelling of `0.0.0.0/8` is not screened, and the sharpest case was
/// the bare `0` — `Url::parse("http://0/")` is `0.0.0.0`, so `0/admin` and
/// `0:8080/x` reached it. That was **pre-existing** (`is_plausible_host("0")`
/// was already `false`) rather than opened by F-36, and **schemeless-only**:
/// this predicate has exactly one production call site, in the bare-authority
/// scan, so a scheme-bearing `http://0/admin` was always screened.
///
/// **User decision 2026-08-12: closed anyway.** `0.0.0.0` is excluded from the
/// carve-out — see [`is_prose_shaped_ipv4`]. It is the shortest payload in the
/// very class F-36 closed, and leaving it meant shipping a test that asserts a
/// bypass passes. **Accepted price, measured over 41 realistic strings rather
/// than guessed: `"0:00 UTC"`, `"0/10 tests passed"` and `"the match ended 0:0"`
/// now deny.** `a_short_zero_form_is_prose_and_the_full_spelling_is_a_target`
/// pins both halves, so the trade stays visible instead of becoming folklore.
///
/// # Trailing dots
///
/// WHATWG removes **one** trailing empty part before parsing, so `127.0.0.1.` is
/// a four-part literal and not a three-part short form. Counting parts without
/// that removal would hand the trailing-dot bypass right back.
pub(super) fn is_prose_shaped_ipv4(host: &str, addr: Ipv4Addr) -> bool {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    // `0.0.0.0` itself is NEVER prose (#48, F-36 residual, user decision
    // 2026-08-12). Every other zero-first-octet short form still is: the
    // carve-out exists for `0.5:1`, `0.1.2.3` and friends, whose left-padded
    // addresses land harmlessly inside `0.0.0.0/8` — but the bare `0` reads as
    // `0.0.0.0` exactly, which reaches localhost on Linux and is one of the
    // shortest SSRF payloads there is.
    //
    // Measured price, accepted knowingly rather than guessed: `"0:00 UTC"`,
    // `"0/10 tests passed"` and `"the match ended 0:0"` now deny. That is the
    // whole cost — a short form can only read as exactly `0.0.0.0` when every
    // part is zero, so no other prose shape is reachable from here.
    parts.len() < 4 && addr.octets()[0] == 0 && !addr.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address parses")
    }

    /// Every range in the locked deny set, plus the neighbours that must stay
    /// reachable. The out-of-range cases are the real test: an over-broad
    /// screen ("anything starting with 172") would break legitimate research
    /// just as silently as a hole would let an attack through.
    #[test]
    fn is_denied_ip_covers_every_locked_range_and_no_more() {
        for denied in [
            // 10/8
            "10.0.0.0",
            "10.255.255.255",
            "10.1.2.3",
            // 172.16/12
            "172.16.0.0",
            "172.31.255.255",
            "172.21.1.11",
            // 192.168/16
            "192.168.0.1",
            "192.168.255.255",
            // 127/8
            "127.0.0.1",
            "127.255.255.254",
            // link-local + cloud metadata
            "169.254.0.1",
            "169.254.169.254",
            // CGNAT
            "100.64.0.1",
            "100.127.255.255",
            // "this network"
            "0.0.0.0",
            "0.1.2.3",
            // v6
            "::1",
            "::",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            // IPv4-mapped v6 — the case a v4-only screen misses.
            "::ffff:10.0.0.1",
            "::ffff:192.168.1.1",
            "::ffff:127.0.0.1",
            // IPv4-compatible v6 (deprecated form).
            "::127.0.0.1",
            "::10.0.0.1",
            // #48: NAT64 (RFC 6052 well-known prefix) and 6to4 (RFC 3056),
            // unmapped and re-checked like every other embedded-v4 form.
            "64:ff9b::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::10.0.0.1",
            "64:ff9b::169.254.169.254",
            "2002:7f00:1::",
            "2002:7f00:1::1",
            "2002:c0a8:101::",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(is_denied_ip(ip(denied)), "{denied} must be denied");
        }
        for allowed in [
            // Just outside each v4 range.
            "9.255.255.255",
            "11.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.1",
            "169.253.255.255",
            "169.255.0.0",
            "1.1.1.1",
            "8.8.8.8",
            // Public v6, and the neighbours of fe80::/10 and fc00::/7.
            "2001:db8::1",
            "2606:4700:4700::1111",
            "fec0::1",
            "fe7f::1",
            "fe00::1",
            "::ffff:8.8.8.8",
            // #48: the embedded-v4 prefixes are unmapped, not blanket-denied,
            // so a PUBLIC destination reached through NAT64 or 6to4 stays
            // reachable — exactly as `::ffff:8.8.8.8` does.
            "64:ff9b::8.8.8.8",
            "2002:0808:0808::",
            // …and the neighbours of both prefixes are ordinary public v6.
            "64:ff9a::1",
            "64:ff9c::1",
            "0064:ff9b:1::7f00:1",
            "2001::1",
            "2003::1",
        ] {
            assert!(!is_denied_ip(ip(allowed)), "{allowed} must be allowed");
        }
    }

    /// The widened extractor, checked directly: what it emits and, just as
    /// importantly, what it does not. `extract_urls` is public and
    /// `detection::first_url` reads its first element for a row's target, so
    /// the ordering — scheme-bearing runs first, as written before stripped —
    /// is a contract, not an accident.
    #[test]
    fn extraction_is_widened_deduplicated_and_ordered() {
        // A scheme'd run still comes first, so `detection::first_url` is stable.
        let urls = extract_urls(&json!({ "u": "http://\t127.0.0.1:12344/props" }));
        assert_eq!(
            urls,
            vec!["http://", "http://127.0.0.1:12344/props"],
            "the truncation, then the run a parser actually sees"
        );

        // Both scan variants see the same runs across a newline; the union is
        // deduplicated so one target is not resolved twice.
        let urls = extract_urls(&json!({ "u": "http://a.example/1 \n http://b.example/2" }));
        assert_eq!(urls, vec!["http://a.example/1", "http://b.example/2"]);

        // Stripping alone would glue these into one run whose host is the
        // FIRST url; scanning as-written too is what keeps the second visible.
        let urls = extract_urls(&json!({ "u": "http://a.example/x\nhttp://10.0.0.1/" }));
        assert!(urls.contains(&"http://10.0.0.1/".to_string()), "{urls:?}");

        // #48 finding H-3: every spelling of the scheme — case, and any slash
        // run including none — normalizes to ONE candidate. The last two are
        // also the double-scan check: `\` still splits words for
        // `scan_bare_authorities`, so those strings are seen by both scans, and
        // lowercasing the scheme is what makes the two agree on a single
        // string instead of resolving one target twice.
        for spelling in [
            "http://127.0.0.1:12344/props",
            "http:/127.0.0.1:12344/props",
            "http:127.0.0.1:12344/props",
            "HTTP:127.0.0.1:12344/props",
            "http:///127.0.0.1:12344/props",
            "HTTP:\\\\127.0.0.1:12344/props",
            "hTtP:/\\127.0.0.1:12344/props",
        ] {
            assert_eq!(
                extract_urls(&json!({ "u": spelling })),
                vec!["http://127.0.0.1:12344/props"],
                "{spelling:?}"
            );
        }
        // …and `\` inside the run is a path separator for a special scheme, not
        // a terminator, so the run survives it whole.
        assert_eq!(
            extract_urls(&json!({ "u": "http://127.0.0.1\\props" })),
            vec!["http://127.0.0.1\\props"]
        );

        // Schemeless and protocol-relative runs are normalized with the scheme
        // a guessing fetcher would assume.
        assert_eq!(
            extract_urls(&json!({ "u": "127.0.0.1:8080/admin" })),
            vec!["http://127.0.0.1:8080/admin"]
        );
        assert_eq!(
            extract_urls(&json!({ "u": "//169.254.169.254/latest" })),
            vec!["http://169.254.169.254/latest"]
        );

        // #48 M-18, at the extractor rather than through the screen: a canonical
        // network address produces NO candidate, and each near miss produces one.
        assert!(
            extract_urls(&json!({ "u": "RFC1918 (10.0.0.0/8)" })).is_empty(),
            "{:?}",
            extract_urls(&json!({ "u": "RFC1918 (10.0.0.0/8)" }))
        );
        for (label, arg, expect) in [
            ("host bits set", "10.0.0.1/8", "http://10.0.0.1/8"),
            ("a single host", "10.0.0.0/32", "http://10.0.0.0/32"),
            ("a port", "10.0.0.0:80/8", "http://10.0.0.0:80/8"),
            ("a further segment", "10.0.0.0/8/x", "http://10.0.0.0/8/x"),
            ("protocol-relative", "//10.0.0.0/8", "http://10.0.0.0/8"),
        ] {
            assert_eq!(
                extract_urls(&json!({ "u": arg })),
                vec![expect],
                "{label}: {arg:?}"
            );
        }
        // Scheme-bearing is `scan_scheme_runs`'s business and never exempt.
        assert_eq!(
            extract_urls(&json!({ "u": "http:10.0.0.0/8" })),
            vec!["http://10.0.0.0/8"]
        );
    }

    #[test]
    fn extract_urls_finds_them_at_any_depth_and_leaves_prose_alone() {
        let args = json!({
            "query": "compare frameworks",
            "sources": [
                "https://example.org/a",
                { "href": "http://example.net:8080/b?q=1" },
            ],
            "nested": { "deep": { "note": "see https://docs.example.com/c for details." } },
            "count": 7,
            "flag": true,
        });
        let urls = extract_urls(&args);
        assert!(urls.contains(&"https://example.org/a".to_string()));
        assert!(urls.contains(&"http://example.net:8080/b?q=1".to_string()));
        // Trailing sentence punctuation is not part of the URL.
        assert!(urls.contains(&"https://docs.example.com/c".to_string()), "{urls:?}");
        assert_eq!(urls.len(), 3, "{urls:?}");

        // Nothing URL-shaped ⇒ nothing extracted; keys are never scanned.
        let plain = json!({
            "https://not-a-value.example": "just a key",
            "text": "no links here, only the word http and a bare example.org",
            "path": "C:/repo/src/main.rs",
        });
        assert!(extract_urls(&plain).is_empty(), "{:?}", extract_urls(&plain));
    }

    #[test]
    fn extract_urls_handles_several_in_one_string() {
        let args = json!({ "q": "http://a.example/1 and https://b.example/2, plus \"http://c.example/3\"" });
        let urls = extract_urls(&args);
        assert_eq!(
            urls,
            vec![
                "http://a.example/1",
                "https://b.example/2",
                "http://c.example/3",
            ]
        );
    }
}

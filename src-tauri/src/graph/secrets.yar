/*
  cImp — the memory secret screen (V32 Phase C2, #48).

  These rules run against ONE short string: the text of a `context_note` write,
  before it enters project memory. They never run against a file, a fetched page
  or a tool result — `offload/detection/rules.d/*.yar` owns that surface, and
  this file must not duplicate it.

  Curation rules for anything added here:

  - **Precision over recall.** A hit quarantines the note (it is stored and held
    for review, never dropped), but a false positive still costs the user a trip
    to the Memory view. Prefer a rule anchored on a vendor prefix or a
    structural shape over one that keys on an English word.
  - **Every rule identifier starts with `secret_`.** The screen is compiled from
    this file alone, so the prefix is documentation rather than a filter — but
    the note the model is shown names the identifiers, and they are read by a
    human.
  - **Add a positive AND a negative sample** to `secrets.rs`'s tests for every
    rule. The negative half is the one that matters: `benign_notes_do_not_match`
    is what stops this file from quietly eating research conclusions.
*/

rule secret_private_key_block {
    meta:
        label = "a PEM private-key block"
    strings:
        $pem = /-----BEGIN [A-Z0-9 ]{0,24}PRIVATE KEY( BLOCK)?-----/
    condition:
        $pem
}

rule secret_aws_access_key_id {
    meta:
        label = "an AWS access key id"
    strings:
        $id = /(AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}/
    condition:
        $id
}

rule secret_github_token {
    meta:
        label = "a GitHub token"
    strings:
        $gh = /gh[pousr]_[A-Za-z0-9]{36,80}/
    condition:
        $gh
}

rule secret_slack_token {
    meta:
        label = "a Slack token"
    strings:
        $x = /xox[abcprs]-[A-Za-z0-9-]{12,120}/
    condition:
        $x
}

rule secret_anthropic_api_key {
    meta:
        label = "an Anthropic API key"
    strings:
        $k = /sk-ant-[A-Za-z0-9_-]{24,120}/
    condition:
        $k
}

rule secret_openai_style_api_key {
    meta:
        label = "an OpenAI-style API key"
    strings:
        $k = /sk-[A-Za-z0-9]{32,80}/
    condition:
        $k
}

rule secret_google_api_key {
    meta:
        label = "a Google API key"
    strings:
        $k = /AIza[0-9A-Za-z_-]{35}/
    condition:
        $k
}

rule secret_stripe_key {
    meta:
        label = "a Stripe key"
    strings:
        $k = /[sr]k_(live|test)_[0-9A-Za-z]{16,80}/
    condition:
        $k
}

rule secret_json_web_token {
    meta:
        label = "a JSON Web Token"
    strings:
        $j = /eyJ[A-Za-z0-9_-]{8,400}\.[A-Za-z0-9_-]{8,400}\.[A-Za-z0-9_-]{8,400}/
    condition:
        $j
}

rule secret_assigned_credential {
    meta:
        label = "a credential assigned to a quoted value"
    strings:
        /*
          The QUOTES are what make this rule safe to ship: prose about a secret
          ("the API key lives in .env, never in the repo") has no quoted value
          after a `:` or `=`, so it does not match, while a pasted config line
          does.
        */
        $a = /(api[_-]?key|secret|password|passwd|passphrase|client[_-]?secret|access[_-]?token|auth[_-]?token)["']?[ \t]*[:=][ \t]*["'][^"'\r\n]{16,200}["']/ nocase
    condition:
        $a
}

rule secret_bearer_credential {
    meta:
        label = "an Authorization bearer credential"
    strings:
        $b = /bearer [A-Za-z0-9_\-.=]{20,200}/ nocase
    condition:
        $b
}

rule secret_url_with_password {
    meta:
        label = "a URL carrying an inline password"
    strings:
        $u = /[a-z][a-z0-9+.-]{1,20}:\/\/[^\/ \t\r\n:@"']{1,64}:[^\/ \t\r\n:@"']{6,64}@/
    condition:
        $u
}

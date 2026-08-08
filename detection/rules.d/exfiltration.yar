/*
 * cImp V32 — signature screen, exfiltration and tool-steering families.
 *
 * These are the rules that map onto the milestone's threat model directly:
 * the lethal trifecta is private-data access + untrusted content + an
 * exfiltration channel, and an injected page's payload is almost always a
 * directive that supplies the third leg ("read X, then fetch <url>?q=X").
 *
 * The taint latch (Phase A/B) is what *stops* that; these rules are what makes
 * it visible when someone tries. See ../README.md for provenance (Vigil
 * `mdexfil.yar`, Embrace The Red's Bing Chat image-exfiltration PoC).
 */

rule CImp_Exfil_SendSensitiveDataToUrl
{
    meta:
        family      = "exfiltration"
        severity    = "high"
        description = "Directive to transmit context, secrets or credentials to a URL"
        provenance  = "cImp; the canonical indirect-injection payload shape"
    strings:
        // An imperative that ships something to an http(s) endpoint, on one line.
        $to_url = /(send|post|upload|transmit|exfiltrate|forward|submit|deliver|report|leak|beacon)[^\n]{0,60}https?:\/\/[^\s"'`<>]{4,}/ nocase
        // …and something worth shipping named nearby in the same result.
        $secret = /(system prompt|your instructions|initial prompt|conversation history|chat history|context window|api[ _-]?key|access[ _-]?token|bearer token|secret key|private key|credential|password|id_rsa|\.ssh|\.env\b|aws[ _-]?(access|secret))/ nocase
    condition:
        $to_url and $secret
}

rule CImp_Exfil_MarkdownImageQuery
{
    meta:
        family      = "exfiltration"
        severity    = "high"
        description = "Markdown image/link whose URL carries a query parameter — the zero-click exfil channel"
        provenance  = "widened re-derivation of Vigil MarkdownExfiltration (deadbits/vigil-llm, Apache-2.0), which cites embracethered.com"
    strings:
        // ![alt](https://host/path?q=…  — the rendered image fires the request
        // with whatever the model interpolated into the query string.
        //
        // The parameter NAME is the discriminator, not the mere presence of a
        // query string: badge, CDN and analytics images legitimately carry
        // `?style=`, `?color=`, `?w=`, `?v=` and friends on ordinary pages, so
        // matching "image URL with any parameter" would fire on a large share
        // of README-shaped content. Restricted to the short, contentless names
        // an exfiltration sink actually uses.
        $img  = /!\[[^\]\n]{0,60}\]\([ \t]{0,2}https?:\/\/[^\s)]{4,}[?&](q|d|c|p|s|x|data|out|dump|payload|leak|info|content|context|prompt|result|summary|secret|key|token|msg|text|body)=/ nocase
        // The instruction that plants one, for pages that describe rather than embed.
        $tell = /(render|display|include|insert|output|embed|show)[^\n]{0,50}(markdown\s{1,8})?(image|img|!\[)[^\n]{0,60}(with|containing|encode|append|include)[^\n]{0,40}(the\s{1,8})?(summary|content|data|context|instructions|secret|result)/ nocase
    condition:
        any of them
}

rule CImp_Exfil_EncodeIntoUrl
{
    meta:
        family      = "exfiltration"
        severity    = "high"
        description = "Directive to encode data into a URL, query string or parameter"
        provenance  = "cImp; the obfuscated variant of CImp_Exfil_SendSensitiveDataToUrl"
    strings:
        $encode = /(append|encode|embed|include|place|put|add|base64|url-?encode|hex-?encode)[^\n]{0,50}(in|into|to|as|within)\s{1,8}(the\s{1,8})?(url|uri|link|query[ \t-]?(string|param|parameter)?|parameter|path|fragment|hostname|subdomain)/ nocase
        $what   = /(system prompt|your instructions|conversation|context|history|file\s{1,8}content|the\s{1,8}(contents|output|result)|secret|key|token|credential)/ nocase
    condition:
        $encode and $what
}

rule CImp_Exfil_ImperativeFetchWithPayloadParam
{
    meta:
        family      = "exfiltration"
        severity    = "high"
        description = "An imperative fetch of a URL whose query parameter is a data sink"
        provenance  = "cImp; matches the milestone's own live-verification payload shape"
    strings:
        $fetch = /(fetch|curl|wget|visit|browse|navigate\s{1,8}to|open|request|GET|call)\s{1,8}(the\s{1,8}(url|link|page)\s{1,8})?https?:\/\/[^\s"'`<>]{2,}[?&](q|d|c|p|s|x|data|out|dump|payload|leak|info|content|context|prompt|result|secret|key|token)=/ nocase
    condition:
        $fetch
}

rule CImp_ToolSteering_ReadThenReport
{
    meta:
        family      = "tool-steering"
        severity    = "high"
        description = "Content naming cImp's own tools and telling the model to route their output somewhere"
        provenance  = "cImp; the tool names are this app's native/offload tool surface"
    strings:
        // Our own tool names are a strong signal on their own: a fetched page
        // has no legitimate reason to know them.
        $tool = /\b(read_file|list_dir|code_search|run_command|graph_snippet|graph_search_docs|graph_semantic_code|offload_task|context_note)\b/
        $use  = /(use|call|invoke|run|execute|issue|make)\s{1,8}(the\s{1,8}|a\s{1,8})?(read_file|list_dir|code_search|run_command|read|write|edit|bash|shell|terminal|browser|webfetch)[ \t]{0,4}(tool|function|command|call)/ nocase
        $route = /(include|paste|attach|append|add|put|send|return|report)[^\n]{0,50}(content|output|result|text|response|file|answer)[^\n]{0,30}(in|into|to|within)[^\n]{0,25}(the\s{1,8})?(url|link|query|request|message|comment|search|next\s{1,8}(call|fetch))/ nocase
    condition:
        ($tool and $route) or ($use and $route)
}

rule CImp_ToolSteering_SecretFileRead
{
    meta:
        family      = "tool-steering"
        severity    = "high"
        description = "Directive to read a well-known credential or key file"
        provenance  = "cImp; the read half of read-then-exfiltrate"
    strings:
        $read = /(read|open|cat|print|show|display|retrieve|fetch|load|access|dump|contents\s{1,8}of)[^\n]{0,50}(~\/\.ssh\/|\/\.ssh\/|id_rsa|id_ed25519|\.env\b|\.aws\/credentials|\.npmrc|\.git-credentials|\/etc\/(passwd|shadow)|credentials\.json|secrets?\.(json|yaml|yml|toml)|\.pem\b)/ nocase
    condition:
        $read
}

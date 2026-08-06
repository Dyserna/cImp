/*
 * cImp V32 — signature screen, obfuscation and hidden-channel families.
 *
 * Payloads that a human reviewing the page would not see, but the model reads
 * anyway: text hidden from the rendered view, characters that are invisible by
 * definition, and instructions moved out of plain sight behind an encoding.
 *
 * These rules are the ones most likely to be *the only* signal on a
 * professionally built injection page — the plain-language rules in
 * injection_core.yar match the payload's words, these match its delivery.
 * See ../README.md for provenance (garak `encoding` probes; Embrace The Red's
 * Unicode-tag smuggling write-ups).
 */

rule CImp_Obfuscation_ZeroWidthRun
{
    meta:
        family      = "hidden-text"
        severity    = "medium"
        description = "A dense run of zero-width characters — invisible to the reader, tokens to the model"
        provenance  = "cImp; ZWSP/ZWNJ/ZWJ/word-joiner steganography"
    strings:
        // U+200B ZWSP, U+200C ZWNJ, U+200D ZWJ, U+2060 word joiner, in UTF-8.
        // Single occurrences are ordinary (ZWJ builds emoji, ZWNJ is required
        // in Persian/Hindi text), so the *count* is the signal.
        $zw = { ( e2 80 8b | e2 80 8c | e2 80 8d | e2 81 a0 ) }
    condition:
        #zw > 24
}

rule CImp_Obfuscation_UnicodeTagSmuggling
{
    meta:
        family      = "hidden-text"
        severity    = "high"
        description = "Unicode tag-block characters (U+E0000..) used to smuggle text past every renderer"
        provenance  = "embracethered.com ASCII-smuggling research; garak encoding probes"
    strings:
        // U+E0000-U+E03FF in UTF-8 is f3 a0 8x yy. The tag block has no
        // legitimate use in modern text at all — it was deprecated in
        // Unicode 5.1 — so even a short run is worth flagging.
        $tag = { f3 a0 8? ?? }
    condition:
        #tag > 8
}

rule CImp_Obfuscation_HtmlCommentImperative
{
    meta:
        family      = "hidden-text"
        severity    = "high"
        description = "An injection imperative parked inside an HTML comment"
        provenance  = "cImp; the cheapest way to hide a payload from a rendered page"
    strings:
        $c = /<!--[^>]{0,300}(ignore[ \t]{1,4}(all[ \t]{1,4}|the[ \t]{1,4})?(previous|prior|above)|you[ \t]{1,4}are[ \t]{1,4}now|new[ \t]{1,4}instructions|system[ \t]{1,4}prompt|assistant[ \t]{0,2}:|do[ \t]{1,4}not[ \t]{1,4}tell[ \t]{1,4}the[ \t]{1,4}user|send[ \t]{1,4}(it|them|the)[ \t]{1,4}to)/ nocase
    condition:
        $c
}

rule CImp_Obfuscation_VisuallyHiddenImperative
{
    meta:
        family      = "hidden-text"
        severity    = "medium"
        description = "Text hidden by CSS in the same document as an injection imperative"
        provenance  = "cImp; display:none / font-size:0 / off-screen payload placement"
    strings:
        $hidden = /(display[ \t]{0,2}:[ \t]{0,2}none|visibility[ \t]{0,2}:[ \t]{0,2}hidden|font-size[ \t]{0,2}:[ \t]{0,2}0(px|pt|em|rem)?[ \t]{0,2}[;"']|opacity[ \t]{0,2}:[ \t]{0,2}0[ \t]{0,2}[;"']|text-indent[ \t]{0,2}:[ \t]{0,2}-[0-9]{3,}|left[ \t]{0,2}:[ \t]{0,2}-[0-9]{4,})/ nocase
        $imper  = /(ignore[ \t]{1,4}(all[ \t]{1,4}|the[ \t]{1,4})?(previous|prior|above)[ \t]{1,4}(instruction|prompt)|you[ \t]{1,4}are[ \t]{1,4}now[ \t]{1,4}|from[ \t]{1,4}now[ \t]{1,4}on[, \t]|new[ \t]{1,4}instructions[ \t]{0,2}:|system[ \t]{1,4}prompt[ \t]{0,2}:|do[ \t]{1,4}not[ \t]{1,4}tell[ \t]{1,4}the[ \t]{1,4}user)/ nocase
    condition:
        $hidden and $imper
}

rule CImp_Obfuscation_DecodeThenExecute
{
    meta:
        family      = "encoded-payload"
        severity    = "high"
        description = "An encoded blob accompanied by a directive to decode and act on it"
        provenance  = "garak encoding probe family (base64/rot13/hex injection carriers)"
    strings:
        // The directive half, in either order ("decode … then follow" /
        // "follow the base64 below").
        $decode_then = /(base64[ \t-]{0,2}decode|b64decode|atob|decode[ \t]{1,4}(the[ \t]{1,4})?(following|below|this|string|text|payload|message)|from[ \t]{1,4}base64|rot13)[^\n]{0,80}(and[ \t]{1,4}|then[ \t]{1,4}|,[ \t]{1,4})(execute|run|follow|obey|do|perform|apply|comply|act)/ nocase
        $exec_encoded = /(execute|run|follow|obey|perform|apply|comply[ \t]{1,4}with)[^\n]{0,40}(the[ \t]{1,4})?(following[ \t]{1,4}|below[ \t]{1,4}|encoded[ \t]{1,4}|obfuscated[ \t]{1,4})?(base64|rot13|hex[ \t]{1,4}string|encoded[ \t]{1,4}(instruction|command|payload|message))/ nocase
    condition:
        // Deliberately NOT "a long base64 blob is present": data URIs, inline
        // images and embedded certificates make long blobs ordinary on real
        // pages, and a blob without a directive steers nothing. The directive
        // is the payload; the blob is just its luggage.
        any of them
}

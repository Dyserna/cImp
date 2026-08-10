/*
 * cImp V32 — signature screen, core prompt-injection families.
 *
 * Scanned against the RAW text of EXTERNAL tool results (fetched pages, docs
 * lookups). A match only warns: it prepends a header and writes an
 * `injection_flag` Tool Activity row. See ../README.md for provenance
 * (Vigil `instruction_bypass.yar` / `system_instructions.yar`, garak's
 * `promptinject` and `dan` probe families) and the false-positive contract.
 *
 * Tuning stance: SPECIFIC, not sensitive. Every rule here requires the full
 * imperative shape, not a topic word — an article that merely uses the terms
 * "prompt injection", "jailbreak" or "system prompt" must not fire.
 */

rule CImp_Injection_IgnorePreviousInstructions
{
    meta:
        family      = "instruction-override"
        severity    = "high"
        description = "Classic instruction-bypass phrasing aimed at the reading model"
        provenance  = "derived from Vigil InstructionBypass (deadbits/vigil-llm, Apache-2.0)"
    strings:
        // "ignore all previous instructions", "disregard the above rules",
        // "forget your prior directions". The noun is required — "ignore the
        // previous section" (ordinary prose) does not match.
        $override = /(ignore|disregard|forget|discard|override|bypass)\s{1,8}(all\s{1,8}|any\s{1,8}|the\s{1,8}|your\s{1,8}|of\s{1,8}){0,3}(previous|prior|above|earlier|preceding|foregoing|initial|original|prev)\s{1,8}(and\s{1,8}[a-z]{1,12}\s{1,8}){0,1}(instruction|prompt|direction|directive|rule|command|guideline|guidance|constraint)/ nocase
        // "everything above this line is void / no longer applies".
        $void = /(everything|anything|all)[^\n]{0,30}(above|before this|preceding)[^\n]{0,30}(is|are)[^\n]{0,20}(void|invalid|obsolete|a test|no longer (valid|applies|applicable))/ nocase
    condition:
        any of them
}

rule CImp_Injection_NewInstructionBlock
{
    meta:
        family      = "instruction-override"
        severity    = "high"
        description = "Content asserting that instructions embedded in it supersede the real ones"
        provenance  = "derived from Vigil system_instructions.yar"
    strings:
        // A labelled instruction block planted in data.
        $label = /(^|\n)[ \t>*#-]{0,8}(new|updated|revised|additional|urgent|important|priority|override)\s{1,8}(system\s{1,8})?(instruction|directive|prompt|rule|task|command)s?[ \t]{0,4}[:\-]/ nocase
        // An explicit precedence claim.
        $precede = /(these|the following|the instructions below)[^\n]{0,40}(supersede|override|replace|take precedence over|outrank)[^\n]{0,40}(previous|prior|earlier|all other|your (original|system))/ nocase
    condition:
        any of them
}

rule CImp_Injection_FakeSystemTurn
{
    meta:
        family      = "role-forgery"
        severity    = "high"
        description = "Chat-template or role markers forged inside data to fake a system/developer turn"
        provenance  = "cImp; template markers from the llama.cpp / ChatML / Llama-3 formats"
    strings:
        $chatml   = "<|im_start|>system" nocase
        $llama3   = "<|start_header_id|>system" nocase
        $tagged   = /<\|(system|im_start)\|>/ nocase
        $bracket  = /(^|\n)[ \t>*#-]{0,8}\[(SYSTEM|SYSTEM PROMPT|INST|ADMIN|DEVELOPER)\][ \t]{0,4}[:\-]?/ nocase
        $markdown = /(^|\n)[ \t>*#-]{0,8}(###[ \t]{0,4})?(system|developer)[ \t]{0,2}(message|prompt|instruction)?[ \t]{0,2}:[ \t]{0,4}(you|ignore|from now|the user|your)/ nocase
        $jsonrole = /"role"[ \t]{0,4}:[ \t]{0,4}"system"/ nocase
        $begin    = /(begin|start)\s{1,8}(of\s{1,8})?system\s{1,8}(prompt|message|instruction)/ nocase
    condition:
        any of them
}

rule CImp_Injection_AuthorityClaim
{
    meta:
        family      = "role-forgery"
        severity    = "medium"
        description = "Data claiming to speak for the system, the developer or the model vendor"
        provenance  = "garak promptinject / dan phrasing families (NVIDIA/garak, Apache-2.0)"
    strings:
        $from = /(this is|i am|message)[^\n]{0,25}(from|by)\s{1,8}(the\s{1,8})?(system|developer|administrator|admin|operator|openai|anthropic|your (creator|developer|owner|vendor|maker))\b/ nocase
        $auth = /(as|with)\s{1,8}(the\s{1,8})?(system|root|admin|developer)\s{1,8}(authority|privilege|access|override)/ nocase
    condition:
        any of them
}

rule CImp_Injection_RoleReassignment
{
    meta:
        family      = "role-reassignment"
        severity    = "high"
        description = "Attempt to redefine the reading model's persona or operating mode"
        provenance  = "garak dan probe family (DAN, developer mode, AIM) — re-derived phrasings"
    strings:
        $younow  = /you\s{1,8}are\s{1,8}(now|no longer)\s{1,8}(a|an|the|not)?[ \t]{0,4}[a-z]{2,20}/ nocase
        $fromnow = /from\s{1,8}now\s{1,8}on[,\s]{1,4}you\s{1,8}(are|will|must|shall|should|act|respond|answer)/ nocase
        $pretend = /(pretend|act as if|imagine|roleplay|role-play)\s{1,8}(that\s{1,8})?you\s{1,8}(are|have|were)[^\n]{0,40}(no|not|unrestricted|unfiltered|without)/ nocase
        $dan     = /\b(DAN|AIM|STAN|DUDE)\s{1,8}(mode|prompt|jailbreak)\b/
        $anynow  = /\bdo\s{1,8}anything\s{1,8}now\b/ nocase
        $devmode = /((enter|activate|enable|switch to|you are in)\s{1,8}(the\s{1,8})?)(developer|dev|god|admin|debug|unrestricted|unfiltered|jailbreak|sudo)\s{1,8}mode/ nocase
    condition:
        // Two independent signals, or one of the unambiguous jailbreak names.
        $dan or $anynow or $devmode or ($younow and $fromnow) or ($pretend and ($younow or $fromnow))
}

rule CImp_Injection_ConstraintRemoval
{
    meta:
        family      = "guardrail-removal"
        severity    = "high"
        description = "Instructions telling the model its safety rules no longer apply"
        provenance  = "garak dan / jailbreak corpora — re-derived"
    strings:
        $none    = /you\s{1,8}(have|now have)\s{1,8}no\s{1,8}(restrictions|limitations|rules|guidelines|filters|constraints|boundaries)/ nocase
        $without = /(respond|answer|reply|behave|operate|act)[^\n]{0,30}without\s{1,8}(any\s{1,8})?(restrictions|filters|limitations|censorship|refusals|safety|ethical)/ nocase
        $disable = /(ignore|bypass|disable|turn off|switch off|circumvent|skip)[^\n]{0,30}(your\s{1,8}|the\s{1,8})?(safety|content|security|moderation|ethical|alignment)[^\n]{0,20}(polic|filter|guideline|guardrail|restriction|rule|check)/ nocase
        $norefuse = /(never|do not|don't)\s{1,8}(refuse|decline|say\s{1,8}(you\s{1,8})?(can'?t|cannot))/ nocase
    condition:
        any of them
}

rule CImp_Injection_SystemPromptDisclosure
{
    meta:
        family      = "prompt-extraction"
        severity    = "high"
        description = "Directive to reveal the reading model's own system prompt or hidden context"
        provenance  = "derived from Vigil system_instructions.yar; garak leakreplay shapes"
    strings:
        $reveal = /(repeat|print|output|reveal|show|display|reproduce|dump|echo|summari[sz]e|reci[t]e)[^\n]{0,30}(your|the|its)\s{1,8}(entire\s{1,8}|full\s{1,8}|exact\s{1,8}|complete\s{1,8}|original\s{1,8}|initial\s{1,8}|hidden\s{1,8}|secret\s{1,8}|previous\s{1,8})*(system\s{1,8})?(prompt|instruction|message|context|directive|rule)s?\b/ nocase
        $what   = /what\s{1,8}(were|are|was)\s{1,8}(your|the)\s{1,8}([a-z]{1,12}\s{1,8}){0,2}(system\s{1,8})?(prompt|instructions)\b/ nocase
        $above  = /(everything|all\s{1,8}(the\s{1,8})?text)\s{1,8}(above|before)[^\n]{0,20}(verbatim|word[ -]for[ -]word|exactly)/ nocase
    condition:
        any of them
}

rule CImp_Injection_CovertChannel
{
    meta:
        family      = "covert-instruction"
        severity    = "medium"
        description = "Instruction to hide the injected behaviour from the human operator"
        provenance  = "cImp — the shared tail of most real-world indirect-injection payloads"
    strings:
        $hide = /(do\s{1,8}not|don'?t|never)\s{1,8}(tell|inform|mention|reveal|disclose|show|notify|alert|warn)\s{1,8}(this\s{1,8}to\s{1,8})?(the\s{1,8}|your\s{1,8})?(user|human|operator|customer|person)/ nocase
        $silent = /(silently|without\s{1,8}(any\s{1,8})?(mention|notice|comment|acknowledgement|telling))[^\n]{0,40}(comply|obey|follow|execute|perform|do\s{1,8}(this|it|so))/ nocase
    condition:
        any of them
}

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
        $override = /(ignore|disregard|forget|discard|override|bypass)[ \t]{1,4}(all[ \t]{1,4}|any[ \t]{1,4}|the[ \t]{1,4}|your[ \t]{1,4}|of[ \t]{1,4}){0,3}(previous|prior|above|earlier|preceding|foregoing|initial|original|prev)[ \t]{1,4}(and[ \t]{1,4}[a-z]{1,12}[ \t]{1,4}){0,1}(instruction|prompt|direction|directive|rule|command|guideline|guidance|constraint)/ nocase
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
        $label = /^[ \t>*#-]{0,8}(new|updated|revised|additional|urgent|important|priority|override)[ \t]{1,4}(system[ \t]{1,4})?(instruction|directive|prompt|rule|task|command)s?[ \t]{0,4}[:\-]/ nocase
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
        $bracket  = /^[ \t>*#-]{0,8}\[(SYSTEM|SYSTEM PROMPT|INST|ADMIN|DEVELOPER)\][ \t]{0,4}[:\-]?/ nocase
        $markdown = /^[ \t>*#-]{0,8}(###[ \t]{0,4})?(system|developer)[ \t]{0,2}(message|prompt|instruction)?[ \t]{0,2}:[ \t]{0,4}(you|ignore|from now|the user|your)/ nocase
        $jsonrole = /"role"[ \t]{0,4}:[ \t]{0,4}"system"/ nocase
        $begin    = /(begin|start)[ \t]{1,4}(of[ \t]{1,4})?system[ \t]{1,4}(prompt|message|instruction)/ nocase
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
        $from = /(this is|i am|message)[^\n]{0,25}(from|by)[ \t]{1,4}(the[ \t]{1,4})?(system|developer|administrator|admin|operator|openai|anthropic|your (creator|developer|owner|vendor|maker))\b/ nocase
        $auth = /(as|with)[ \t]{1,4}(the[ \t]{1,4})?(system|root|admin|developer)[ \t]{1,4}(authority|privilege|access|override)/ nocase
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
        $younow  = /you[ \t]{1,4}are[ \t]{1,4}(now|no longer)[ \t]{1,4}(a|an|the|not)?[ \t]{0,4}[a-z]{2,20}/ nocase
        $fromnow = /from[ \t]{1,4}now[ \t]{1,4}on[, \t]{1,4}you[ \t]{1,4}(are|will|must|shall|should|act|respond|answer)/ nocase
        $pretend = /(pretend|act as if|imagine|roleplay|role-play)[ \t]{1,4}(that[ \t]{1,4})?you[ \t]{1,4}(are|have|were)[^\n]{0,40}(no|not|unrestricted|unfiltered|without)/ nocase
        $dan     = /\b(DAN|AIM|STAN|DUDE)[ \t]{1,4}(mode|prompt|jailbreak)\b/
        $anynow  = /\bdo[ \t]{1,4}anything[ \t]{1,4}now\b/ nocase
        $devmode = /((enter|activate|enable|switch to|you are in)[ \t]{1,4}(the[ \t]{1,4})?)(developer|dev|god|admin|debug|unrestricted|unfiltered|jailbreak|sudo)[ \t]{1,4}mode/ nocase
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
        $none    = /you[ \t]{1,4}(have|now have)[ \t]{1,4}no[ \t]{1,4}(restrictions|limitations|rules|guidelines|filters|constraints|boundaries)/ nocase
        $without = /(respond|answer|reply|behave|operate|act)[^\n]{0,30}without[ \t]{1,4}(any[ \t]{1,4})?(restrictions|filters|limitations|censorship|refusals|safety|ethical)/ nocase
        $disable = /(ignore|bypass|disable|turn off|switch off|circumvent|skip)[^\n]{0,30}(your[ \t]{1,4}|the[ \t]{1,4})?(safety|content|security|moderation|ethical|alignment)[^\n]{0,20}(polic|filter|guideline|guardrail|restriction|rule|check)/ nocase
        $norefuse = /(never|do not|don't)[ \t]{1,4}(refuse|decline|say[ \t]{1,4}(you[ \t]{1,4})?(can'?t|cannot))/ nocase
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
        $reveal = /(repeat|print|output|reveal|show|display|reproduce|dump|echo|summari[sz]e|reci[t]e)[^\n]{0,30}(your|the|its)[ \t]{1,4}(entire[ \t]{1,4}|full[ \t]{1,4}|exact[ \t]{1,4}|complete[ \t]{1,4}|original[ \t]{1,4}|initial[ \t]{1,4}|hidden[ \t]{1,4}|secret[ \t]{1,4}|previous[ \t]{1,4})*(system[ \t]{1,4})?(prompt|instruction|message|context|directive|rule)s?\b/ nocase
        $what   = /what[ \t]{1,4}(were|are|was)[ \t]{1,4}(your|the)[ \t]{1,4}([a-z]{1,12}[ \t]{1,4}){0,2}(system[ \t]{1,4})?(prompt|instructions)\b/ nocase
        $above  = /(everything|all[ \t]{1,4}(the[ \t]{1,4})?text)[ \t]{1,4}(above|before)[^\n]{0,20}(verbatim|word[ -]for[ -]word|exactly)/ nocase
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
        $hide = /(do[ \t]{1,4}not|don'?t|never)[ \t]{1,4}(tell|inform|mention|reveal|disclose|show|notify|alert|warn)[ \t]{1,4}(this[ \t]{1,4}to[ \t]{1,4})?(the[ \t]{1,4}|your[ \t]{1,4})?(user|human|operator|customer|person)/ nocase
        $silent = /(silently|without[ \t]{1,4}(any[ \t]{1,4})?(mention|notice|comment|acknowledgement|telling))[^\n]{0,40}(comply|obey|follow|execute|perform|do[ \t]{1,4}(this|it|so))/ nocase
    condition:
        any of them
}

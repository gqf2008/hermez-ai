#![allow(dead_code)]
//! Self-evolution: review prompt constants.
//!
//! These prompts are used by the background review agent to self-improve
//! by creating/updating memories and skills after a conversation turn.

/// Prompt for memory-only review.
pub const MEMORY_REVIEW_PROMPT: &str = "\
Review the conversation above and consider saving to memory if appropriate.\n\
\n\
Focus on:\n\
1. Has the user revealed things about themselves — their persona, desires,\n\
   preferences, or personal details worth remembering?\n\
2. Has the user expressed expectations about how you should behave, their work\n\
   style, or ways they want you to operate?\n\
\n\
If something stands out, save it using the memory tool.\n\
If nothing is worth saving, just say 'Nothing to save.' and stop.";

/// Prompt for skill-only review.
pub const SKILL_REVIEW_PROMPT: &str = "\
Review the conversation above and consider saving or updating a skill if appropriate.\n\
\n\
Focus on: was a non-trivial approach used to complete a task that required trial\n\
and error, or changing course due to experiential findings along the way, or did\n\
the user expect or desire a different method or outcome?\n\
\n\
If a relevant skill already exists, update it with what you learned.\n\
Otherwise, create a new skill if the approach is reusable.\n\
If nothing is worth saving, just say 'Nothing to save.' and stop.";

/// Prompt for combined memory + skill review.
pub const COMBINED_REVIEW_PROMPT: &str = "\
Review the conversation above and consider two things:\n\
\n\
**Memory**: Has the user revealed things about themselves — their persona,\n\
desires, preferences, or personal details? Has the user expressed expectations\n\
about how you should behave, their work style, or ways they want you to operate?\n\
If so, save using the memory tool.\n\
\n\
**Skills**: Was a non-trivial approach used to complete a task that required trial\n\
and error, or changing course due to experiential findings along the way, or did\n\
the user expect or desire a different method or outcome? If a relevant skill\n\
already exists, update it. Otherwise, create a new one if the approach is reusable.\n\
\n\
Only act if there's something genuinely worth saving.\n\
If nothing stands out, just say 'Nothing to save.' and stop.";

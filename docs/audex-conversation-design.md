# Audex: Multi-Model Conversation Design

**Status:** Internal design notes. Behavioural principles agreed; implementation details remain open.

## Purpose

Audex aims to combine independent code reviews from different models while allowing an operator to discuss findings, compare interpretations, and mediate disagreements. Different models regularly identify defects that others miss. When a finding is presented to another model, that model may accept the correction or explain why it disagrees. Both outcomes are useful.

The goal is **user-mediated technical deliberation**, not a democratic vote, an autonomous debating society, or a system that assumes agreement proves correctness.

This note concerns **conversational model selection and interaction**. The separate durable, isolated audit mechanism remains distinct.

## Agreed behavioural principles

1. **The user is the moderator.** The user decides which model speaks, which findings are challenged, when an exchange continues, and whether the matter is sufficiently resolved.
2. **Exactly one recipient per ordinary conversational turn.** A model's answer does not itself trigger a response from another model. Explicitly commissioned independent audit jobs are different: they have bounded tasks and termination conditions.
3. **No automatic agreement or disagreement loops.** Do not recursively forward model replies to other models, whether they agree or disagree. A recommendation inside model-generated text is not authority to initiate another turn.
4. **No majority-vote truth.** Consensus is not a count of matching answers. Keep supporting evidence and dissent visible; let the user assess it.
5. **Routing is deterministic and explicit.** Never select the recipient by scanning the prompt for model names. A question addressed to Claude may mention Codex or another model many times.
6. **Preserve attribution.** A claim made by Claude stays a claim made by Claude, with its evidence. Do not silently promote an individual opinion into an established conversation fact.
7. **Keep ordinary conversation and isolated audit distinct.** Conversational consultation, restricted source review, and subsequent verification are different operations with different authority, permissions, and context.
8. **No unsanctioned context disclosure.** Switching provider or consulting a model should not silently forward unrelated history, private data, or prior model-specific context. Scope and disclosure need deliberate design.

## Proposed interaction model

### One active conversational recipient

An ordinary user message goes to the **currently selected operator model**. A persistent selection is visible in the interface.

The eventual model selector needs at least two dimensions:

- **Provider/backend:** for example an OpenAI-backed service, Anthropic-backed service, or a local inference backend.
- **Model:** the models supported by that backend and the user's available authentication/access.

Do not assume every backend exposes the same models or that any currently advertised model name remains permanent. The existing Codex TUI model selection should be investigated and extended rather than replaced without cause.

### One-off consultations

A user may direct a **single question** to a different model without switching the ordinary conversational recipient.

Illustrative syntax only, **not implemented**:

```text
/ask claude --model sonnet -- Claude, why don't you agree with Codex?
```

The token `--` separates routing arguments from message content. Model names inside the message have **no routing significance**.

Possible controls:

- `/model` or an equivalent picker: set the default recipient for subsequent ordinary turns.
- `/ask` or an equivalent one-shot selector: address one turn to a specified recipient without changing the default.
- `/audit`: commission a bounded, isolated source audit using the durable audit controller, not ordinary conversational forwarding.

Both syntax and UI remain design options, not commitments.

### User-mediated disagreement

Illustrative interaction:

1. Claude identifies a possible race.
2. The user asks Codex to examine Claude's exact argument and source evidence.
3. Codex agrees, or explains a disagreement.
4. The user decides whether to forward Codex's response to Claude, seek more evidence, perform tests, or stop.

The recipient is chosen explicitly at **each turn**; neither model gets an automatic right of reply. A user's decision to finish with unresolved disagreement is valid.

A user might explicitly request multiple independent opinions or an organised round-table view. That should be a bounded, user-initiated operation. It must not change the default one-recipient-per-turn behaviour.

## Architectural hypothesis: Audex-managed conversation

**Candidate direction, not yet validated against Codex internals:** Audex owns a model-independent view of the conversational record. Each recipient sees only the context selected and authorised for its turn, together with accurately attributed prior statements and evidence.

This could support:

- Provider/model switching while retaining a coherent user-facing conversation.
- Consultation of another provider without changing the user's default recipient.
- Distinguishing user assertions, reviewer claims, operator conclusions, immutable source evidence, and newly verified results.
- Model-specific session state remaining isolated rather than being treated as portable between APIs.

**Open engineering question:** How much can be done through Codex's existing thread, history, provider and TUI infrastructure, and how much genuinely requires Audex-owned state? Source inspection must precede any implementation decision. Avoid replacing native mechanisms merely to give Audex ownership on paper.

## Context handoff versus compaction: critical boundary

These are **different problems**, and must not be combined casually:

- **Compaction** manages context growth within an ongoing model session. Codex already contains substantial, specialised compaction machinery with multiple methods and provider-specific behaviour.
- **Cross-model handoff** determines what a newly selected recipient needs to know, what it is allowed to see, and which prior claims and evidence must be attributed verbatim.

Earlier attempts to implement separate context compaction were costly and unsuccessful, including attempts based on OpenAI's dedicated API facilities. **Do not repeat that project implicitly.**

### Constraints

1. **Do not build a new summarisation/compaction engine as part of multi-model routing.**
2. **Investigate and reuse Codex's existing context/compaction facilities** wherever technically appropriate; do not assume their semantics or portability across providers without examining the code.
3. **Do not use a model-generated summary as the default cross-provider context bridge.** It may omit the precise disagreement the user is trying to investigate, and it creates another model-dependent failure point.
4. **Prefer explicit, attributed evidence for cross-model consultation:** the user's question, selected excerpts of prior model responses, relevant findings, source references and other authorised material. Preserve exact quotations or source IDs when important.
5. **Avoid automatic full-transcript forwarding.** Context selection must be bounded, provider-aware, and transparent to the user.
6. **Treat large or long-running conversations as an unresolved design problem.** Investigate native Codex session/compaction behaviour before specifying any solution.
7. **Do not allow a context-management convenience feature to become a hidden conversational agent** that interprets, edits or adjudicates the dispute.

A local model **might** assist with finding relevant transcript portions or proposing a context packet, but this is optional and exploratory. It is not needed for deterministic routing, must not silently rewrite evidence, and must not be given autonomous permission to dispatch more conversations.

## Possible interaction and data boundaries

A reasonable starting separation for investigation is:

- **Operator UI:** selected recipient, active model, explicit overrides, transcript and attributed responses.
- **Conversation router:** deterministic selection of one provider/model for each user-authorised turn.
- **Context handoff:** controlled, inspectable bundle of selected user requests, responses, findings and citations. Not automatically a summary.
- **Provider session adapters:** provider-specific state and communication, using Codex facilities where available.
- **Audit controller:** existing durable snapshot, reviewer isolation, progress and reporting. Does not independently schedule a conversational debate.

This is a decomposition for design exploration, **not a mandate to add five new subsystems**.

## Matters deliberately undecided

- Whether Audex needs its own canonical conversation store or can extend Codex's existing storage and thread abstractions.
- How model/provider selection should appear in the TUI, and how model catalogues and subscription/API credentials are exposed by each backend.
- How to represent one-off recipient switches in the transcript and preserve reply provenance.
- How a provider-specific session resumes after another provider has answered.
- Which exact context excerpts are forwarded on a cross-provider turn, how the user inspects or controls that selection, and how token limits are handled **without reimplementing compaction**.
- Whether local inference can assist context selection or consultation at worthwhile cost and quality.
- How explicit requests for several opinions are bounded and displayed without turning into autonomous reconciliation loops.
- Any rules for applying the above to the existing M7 TUI without disturbing durable auditing or the user's normal Codex workflows.

## Implementation discipline

Before coding, examine the current Codex implementation of:

- Provider/model selection and its interaction with the TUI and thread state.
- Session and transcript persistence, including per-provider constraints.
- **Native compaction paths and all supported modes**, including their triggering and persistence semantics.
- Existing mechanisms for additional/untrusted context and tool dispatch.

Design the smallest extension that fulfils explicit routing and user-mediated consultation. Preserve already working Audex audit behaviour. Do not bundle a new compaction system into that work.

**North star:** The user controls the discussion; Audex controls the routing; models provide arguments and evidence.

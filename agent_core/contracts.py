"""Compiled business contracts. Authority, paths, secrets and budgets are not inputs."""
from __future__ import annotations

from datetime import date
from typing import Annotated, Literal, Union

from pydantic import BaseModel, ConfigDict, Field, StringConstraints, create_model, model_validator

Ref = Annotated[str, StringConstraints(pattern=r'^ref_[0-9a-f]{32}$')]
ShortText = Annotated[str, StringConstraints(min_length=1, max_length=4096)]


class StrictModel(BaseModel):
    model_config = ConfigDict(strict=True, extra='forbid', frozen=True)


class ScopedQuery(StrictModel):
    query_id: Literal['q0', 'q1', 'q2', 'q3']
    query: Annotated[str, StringConstraints(min_length=1, max_length=4096)]


class DateRange(StrictModel):
    start: Annotated[str, StringConstraints(pattern=r'^\d{4}-\d{2}-\d{2}$')]
    end: Annotated[str, StringConstraints(pattern=r'^\d{4}-\d{2}-\d{2}$')]

    @model_validator(mode='after')
    def ordered(self):
        if date.fromisoformat(self.start) > date.fromisoformat(self.end):
            raise ValueError('Date range must be ordered')
        return self


class SearchInput(StrictModel):
    queries: Annotated[list[ScopedQuery], Field(min_length=1, max_length=4)]
    filter_ids: list[Ref]
    snapshot_id: Ref

    @model_validator(mode='after')
    def original_query(self):
        identifiers = [item.query_id for item in self.queries]
        if identifiers[0] != 'q0' or len(set(identifiers)) != len(identifiers):
            raise ValueError('Exactly one original q0 must be first')
        return self


class GraphRelationsInput(StrictModel):
    seed_ids: Annotated[list[Ref], Field(max_length=32)]
    relation_types: list[Literal['cites', 'amends', 'supersedes', 'evidence_for', 'confirmed_fact', 'derived_from']]
    max_hops: Annotated[int, Field(ge=0, le=2)]
    candidate_set_id: Ref | None = None
    snapshot_id: Ref | None = None

    @model_validator(mode='after')
    def round_binding(self):
        if (self.candidate_set_id is None) != (self.snapshot_id is None):
            raise ValueError('Retrieval expansion needs both its candidate set and snapshot')
        if self.candidate_set_id is not None and self.seed_ids:
            raise ValueError('Expansion seeds are selected by the core from the bound candidate set')
        return self


class SearchPlanInput(StrictModel):
    question_id: Ref
    scope_id: Ref
    constraint_ids: list[Ref]


class TaskPlanInput(StrictModel):
    question_id: Ref
    context_packet_ref: Ref
    catalog_ref: Ref
    goal_ref: Ref
    stop_constraints_ref: Ref


class DraftAnswerInput(StrictModel):
    question_id: Ref
    evidence_bundle_id: Ref
    calculation_ids: list[Ref]
    draft_ref: Ref | None = None
    validation_report_ref: Ref | None = None

    @model_validator(mode='after')
    def paired_revision(self):
        if (self.draft_ref is None) != (self.validation_report_ref is None):
            raise ValueError('A revision requires both draft and validation report')
        return self


class RoleAnalysisInput(StrictModel):
    question_id: Ref
    context_packet_ref: Ref
    evidence_bundle_id: Ref
    calculation_ids: list[Ref]


class RoleCheckInput(RoleAnalysisInput):
    draft_ref: Ref


class ContextBriefInput(StrictModel):
    context_packet_ref: Ref
    progress_refs: list[Ref]
    original_record_refs: Annotated[list[Ref], Field(min_length=1)]
    target_tokens: Annotated[int, Field(ge=1, le=32768)]


class MediaObserveInput(StrictModel):
    media_refs: Annotated[list[Ref], Field(min_length=1, max_length=12)]
    question_id: Ref
    observation_mode: Literal['first_read', 'targeted_compare']
    comparison_ref: Ref | None = None

    @model_validator(mode='after')
    def comparison(self):
        if self.observation_mode == 'targeted_compare' and self.comparison_ref is None:
            raise ValueError('A comparison needs registered extraction and first observation')
        if self.observation_mode == 'first_read' and self.comparison_ref is not None:
            raise ValueError('First read must not receive extraction answers')
        return self


class ContractReviewInput(StrictModel):
    document_versions: list[Ref] = Field(default_factory=list)
    question_id: Ref
    known_constraint_ids: list[Ref]
    media_refs: list[Ref] = Field(default_factory=list)

    @model_validator(mode='after')
    def has_material(self):
        if not self.document_versions and not self.media_refs:
            raise ValueError('At least one document or media version is required')
        return self


class Warning(StrictModel):
    code: ShortText
    message: ShortText


class Coverage(StrictModel):
    processed_refs: list[Ref]
    omitted_refs: list[Ref]
    limitations: list[ShortText]


class Usage(StrictModel):
    generation_attempts: Annotated[int, Field(ge=0, le=7)]
    native_media_attempts: Annotated[int, Field(ge=0, le=2)]
    grounding_units: Annotated[int, Field(ge=0, le=36)]


class SkillResult(StrictModel):
    status: Literal['completed', 'partial', 'needs_input', 'needs_authorization', 'requires_generation', 'cancelled', 'failed', 'installation_fault', 'external_result_unknown']
    root_task_id: ShortText
    invocation_id: ShortText
    skill_id: ShortText
    skill_version: ShortText
    data_ref: Ref | None
    evidence_refs: list[Ref]
    warnings: list[Warning]
    coverage: Coverage
    usage: Usage
    audit_id: ShortText


class SearchPlan(StrictModel):
    question_id: Ref
    scope_id: Ref
    rewritten_queries: Annotated[list[ShortText], Field(max_length=3)]
    constraint_ids: list[Ref]


class TaskNode(StrictModel):
    node_id: ShortText
    depends_on: list[ShortText]
    role_id: Literal['planner', 'evidence-researcher', 'legal-analyst', 'finance-analyst', 'media-reader', 'evidence-checker', 'synthesizer']
    goal_ref: Ref
    acceptance_ref: Ref
    stop_ref: Ref
    context_packet_ref: Ref


class TaskPlan(StrictModel):
    task_graph: Annotated[list[TaskNode], Field(max_length=6)]

    @model_validator(mode='after')
    def acyclic(self):
        nodes = {node.node_id: node for node in self.task_graph}
        if len(nodes) != len(self.task_graph):
            raise ValueError('Duplicate task node')
        visited, active = set(), set()
        def visit(node_id):
            if node_id not in nodes or node_id in active:
                raise ValueError('Invalid or cyclic task graph')
            if node_id in visited:
                return
            active.add(node_id)
            for parent in nodes[node_id].depends_on:
                visit(parent)
            active.remove(node_id)
            visited.add(node_id)
        for node_id in nodes:
            visit(node_id)
        return self


class Claim(StrictModel):
    claim_id: ShortText
    # conclusion / basis / calculation are verified against sources; next_step
    # is advice and is shown as such (design 9.12, lvjin-retrieval contracts 4).
    kind: Literal['conclusion', 'basis', 'calculation', 'next_step']
    text: ShortText
    evidence_refs: list[Ref]
    calculation_refs: list[Ref]


class DraftAnswer(StrictModel):
    question_id: Ref
    claims: list[Claim]
    assumptions: list[ShortText]
    conflicts: list[ShortText]
    unknowns: list[ShortText]


class RoleAnalysis(StrictModel):
    question_id: Ref
    claims: list[Claim]
    proposed_skill_ids: list[ShortText]
    limitations: list[ShortText]


class Challenge(StrictModel):
    claim_id: ShortText
    issue_type: Literal['unsupported', 'contradiction', 'missing_evidence', 'calculation', 'scope']
    evidence_refs: list[Ref]
    proposed_action: ShortText


class ChallengeReport(StrictModel):
    draft_ref: Ref
    challenges: list[Challenge]


class ContextBrief(StrictModel):
    original_record_refs: Annotated[list[Ref], Field(min_length=1)]
    pinned_fact_refs: list[Ref]
    conflict_refs: list[Ref]
    omitted_refs: list[Ref]
    brief: ShortText
    lossy: Literal[True]


class MediaObservation(StrictModel):
    media_refs: Annotated[list[Ref], Field(min_length=1)]
    observations: list[ShortText]
    limitations: list[ShortText]


GENERATION_INPUTS = {
    'search_plan': SearchPlanInput, 'task_plan': TaskPlanInput,
    'draft_answer': DraftAnswerInput, 'role_analysis': RoleAnalysisInput,
    'role_check': RoleCheckInput, 'context_brief': ContextBriefInput,
    'direct_media': MediaObserveInput,
}
GENERATION_OUTPUTS = {
    'search_plan': SearchPlan, 'task_plan': TaskPlan,
    'draft_answer': DraftAnswer, 'role_analysis': RoleAnalysis,
    'role_check': ChallengeReport, 'context_brief': ContextBrief,
    'direct_media': MediaObservation,
}


def field_type(design_type: str):
    """Closed type grammar; unknown design types stop the build."""
    if design_type.endswith('|null'):
        return field_type(design_type[:-5]) | None
    if design_type.startswith('enum(') and design_type.endswith(')'):
        return Literal[tuple(design_type[5:-1].split(','))]
    if design_type == 'integer[0..2]':
        return Annotated[int, Field(ge=0, le=2)]
    if design_type == 'string[1..512]':
        return Annotated[str, StringConstraints(min_length=1, max_length=512)]
    if design_type == 'string':
        return ShortText
    if design_type == 'DateRange':
        return DateRange
    if design_type == 'ScopedQuery[1..4]':
        return Annotated[list[ScopedQuery], Field(min_length=1, max_length=4)]
    if design_type.endswith('[]'):
        return list[field_type(design_type[:-2])]
    if design_type.endswith('Ref'):
        return Ref
    if design_type == 'AllowedRelationType':
        return Literal['cites', 'amends', 'supersedes', 'evidence_for', 'confirmed_fact', 'derived_from']
    if design_type == 'DecodeOutputKind':
        return Literal['frames', 'audio', 'metadata']
    raise ValueError(f'Uncompiled contract type: {design_type}')


def compile_inputs(skills: list[dict]) -> dict[str, type[StrictModel]]:
    fixed = {'knowledge-search': SearchInput, 'graph-relations': GraphRelationsInput, 'query-plan': SearchPlanInput, 'answer-compose': DraftAnswerInput, 'media-observe': MediaObserveInput, 'contract-review': ContractReviewInput}
    result = {}
    for skill in skills:
        if skill['id'] in fixed:
            result[skill['id']] = fixed[skill['id']]
            continue
        fields = {}
        for field in skill['input_fields']:
            value_type = field_type(field['type'])
            default = ... if field['required'] else None
            if not field['required']:
                value_type = value_type | None
            fields[field['name']] = (value_type, Field(default=default, description=field['rule']))
        result[skill['id']] = create_model(skill['id'].replace('-', '_') + '_Input', __base__=StrictModel, **fields)
    return result

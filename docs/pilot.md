# EpocNote usefulness pilot

Use EpocNote for at least two weeks of ordinary agent-assisted development in
one real repository. Record the repository size, harness name and version,
local mode, EpocNote commit or package version, and the first and last dates.
Use the normal project workflow so adoption is measured under the conditions
where the team expects to use the tool.

## Set up

1. Pick a repository and a person to review the pilot evidence.
2. Install EpocNote, initialize the project with the applicable skill target,
   fill in the architecture document, and verify one search and note through
   the chosen client. [Setup guide](../README.md#build-and-run)
3. Record whether the harness discovers the skill and whether it can invoke
   it explicitly. Record observed automatic activation separately.
4. Capture the start date and a baseline `epocnote check` finding count.

## Record the four measures

| Measure | What to record |
| --- | --- |
| Search adoption | Count code-changing sessions and how many invoked `epocnote_search` without a prompt during that session |
| Search usefulness | Have a reviewer sample searches and count how many returned a result used in subsequent work |
| Note quality | Count notes written and how many a reviewer would keep based on their rationale, durability, and originality |
| Staleness | Run `epocnote check` at the end and count all findings |

Publish counts and denominators for each rate, along with the sample size and
selection method. Describe examples in aggregate or with sensitive project
details removed. The pilot can use a private tally of session counts and
review outcomes. It needs no automated session collection.

## Results

A completed report includes the date range, repository size, harness and
version, EpocNote version, the four measurements above, and any differences
between the setup and normal team workflow. The pilot issue stays open
until that report can be based on real use.

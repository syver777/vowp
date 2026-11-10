## Intent

Plan the project job graph and keep the job table consistent.

## Constraints

- no external actions

## Decisions

- fix jobs reuse the job branch — keeps one diff to review (at seq 88)

## State

### Done

- review complete

### In progress

- plan split

### Next

- dispatch workers

## Files

- `docs/design/loops.md` (modified): added L6

## Gotchas

- cargo builds can take several minutes

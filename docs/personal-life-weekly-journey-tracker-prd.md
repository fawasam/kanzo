# PRD: Personal Life, Weekly Planning & Journey Tracker

**Product:** Kanzo

**Status:** Draft

**Owner:** User

**Last updated:** 2026-11-01

## 1. Summary

Extend Kanzo from a shared coding-task board into a private, local-first planning system for work and personal life. The product will help a user connect long-term journeys to weekly intentions and daily actions, while retaining Kanzo's existing Kanban, coding-agent, Git sync, GitHub, and GitLab workflows.

The product should feel like a calm personal operating system, not an enterprise project-management suite or a high-pressure habit tracker.

## 2. Problem

The user manages client work, job search, learning, health, personal commitments, and longer-term goals across separate tools and chat histories. This creates several gaps:

- Daily tasks are disconnected from longer-term goals.
- Weekly planning and review are inconsistent.
- Personal commitments compete invisibly with work tasks.
- Coding agents can work on technical tasks, but there is no clear distinction between agent-ready work and personal actions.
- Progress on a meaningful goal is difficult to see when it is made of many small actions.

## 3. Product goals

1. Give the user one private place to plan today, this week, and longer-term goals.
2. Make the connection between a journey, its milestones, weekly priorities, and daily actions visible.
3. Preserve Kanzo's fast local-first workflow and existing board behavior.
4. Keep personal actions clearly separate from work that coding agents may take.
5. Support a useful weekly reflection without requiring a rigid daily routine.

## 4. Non-goals for the first release

- Replacing a full calendar application or synchronizing Google/Outlook calendars.
- Finance account aggregation, payments, or detailed budgeting.
- Health or medical tracking beyond simple self-reported habits.
- Social features, shared family accounts, public profiles, or cloud-hosted collaboration.
- AI coaching, automatic task generation, or push notifications.
- Removing current project, Kanban, MCP, Git, GitHub, or GitLab functionality.

## 5. Primary user

An individual who balances software/client work with personal goals, learning, health, relationships, and job search. They may use coding agents for implementation work but personally own decisions, life planning, and reflection.

## 6. Product principles

- **Local first:** The board, life plan, and reflections remain in the user's Kanzo data folder and can use the existing Git sync flow.
- **Lightweight by default:** A useful plan should take minutes to maintain, not become another project.
- **Intentional carry-forward:** Unfinished tasks are reviewed and consciously moved forward, rescheduled, or dropped.
- **Private by design:** No account or external service is required for personal planning.
- **Humans choose; agents execute:** Coding agents may take eligible technical tasks, never personal actions unless explicitly changed by the user.

## 7. Core information model

```text
Life Area
  └─ Journey
      └─ Milestone
          └─ Weekly Priority
              └─ Daily Action (Kanzo task)

Habit → daily/weekly check-in
Reflection → daily note or weekly review
```

### 7.1 Life area

A durable part of the user's life, such as Work, Health, Learning, Finance, Family, or Personal. A life area groups journeys and gives the weekly balance view context.

### 7.2 Journey

A longer-term outcome the user wants to pursue. It includes a title, why it matters, optional target date, progress state, associated life area, and milestones.

Examples:

- Career: Become a stronger full-stack engineer.
- Health: Complete a 10K training plan.
- Learning: Finish an advanced AI engineering curriculum.
- Personal: Establish a consistent family connection routine.

### 7.3 Milestone

A measurable checkpoint inside a journey. A milestone may have a target date, status, and linked tasks.

### 7.4 Weekly priority

An explicit intention for a selected week. It can link to a journey/milestone and has a priority rank. The user should normally choose no more than three major priorities for a week.

### 7.5 Daily action

An existing Kanzo task extended with an optional scheduled date and journey/milestone reference. Daily actions remain visible on the Kanban board and can still be work tasks managed by agents.

### 7.6 Habit

A lightweight repeatable behavior: name, life area, cadence, optional target count, and completed check-ins. Habits are not tasks and should not automatically flood the board with cards.

### 7.7 Reflection

A short dated entry for a day or week. It records wins, blockers, what was learned, and one next focus. Reflections belong to the user and are always agent-excluded.

## 8. User experience and views

### 8.1 Today

The default personal-planning view shows:

- Today's date and a quick-capture action.
- Up to three most important actions (MITs).
- Scheduled tasks for today.
- Habit check-ins due today.
- A short daily reflection prompt at the end of the day.
- A link back to the standard Board view for detailed Kanban work.

### 8.2 This Week

The weekly view shows:

- Week range and completion progress.
- Three weekly priorities.
- Tasks grouped by day or unscheduled.
- Linked journey/milestone context.
- A life-area balance summary based on planned actions, not time tracking.
- A one-click start/review flow for a new week.

### 8.3 Journeys

The journeys view lists active and completed journeys by life area. Opening a journey shows its purpose, target date, milestones, recent updates, linked weekly priorities, and linked tasks.

### 8.4 Board

The current Kanban board remains available and continues to be the execution view. Existing projects should map naturally to Work or a chosen life area without breaking the current data model.

### 8.5 Review

The review view supports daily and weekly reflection. Weekly review should summarize completed tasks, habits, journey movement, unfinished tasks, and prompts to decide what moves forward.

## 9. Functional requirements

### 9.1 Life areas

- Users can create, rename, reorder, archive, and color life areas.
- Kanzo ships with optional starter areas: Work, Health, Learning, Personal, Finance, and Relationships.
- An existing project may be associated with one life area; no association is required.

### 9.2 Journeys and milestones

- Users can create, edit, archive, and complete journeys.
- A journey stores: title, description/why, life area, target date, status, and optional progress value.
- Users can create and order milestones within a journey.
- Milestones may be marked planned, active, blocked, or complete.
- Tasks and weekly priorities may link to one journey and optionally one milestone.

### 9.3 Daily and weekly planning

- A task may be scheduled for a date and selected as an MIT.
- The Today view displays scheduled tasks and at most three MITs.
- Users can choose weekly priorities and order them.
- Unfinished scheduled tasks can be moved to another date, retained unscheduled, marked done, or archived during review.
- The system must not automatically carry work forward without a visible user decision.

### 9.4 Habits

- Users can create habits with daily or weekly cadence.
- Users can check a habit in one action from Today or This Week.
- Users can see current streak and completion count for the active week.
- Missing a habit does not create a blocking task or punitive alert.

### 9.5 Reflections and review

- Users can add a dated daily or weekly reflection.
- Weekly review provides a summary of completed tasks, habit completion, completed milestones, and open tasks.
- The user can write: wins, blockers, lesson, and next-week focus.
- Reflections are excluded from agent task selection and MCP tools by default.

### 9.6 Existing Kanzo workflows

- The current Board, Projects, task status flow, comments, attachments, search, Git sync, export, MCP, GitHub repo links, and GitLab MR links continue to work.
- Existing tasks remain valid after migration and appear in Board even when not linked to a life area or journey.
- The existing Human flag is retained. The UI should describe it as **Personal action — agents skip it** in life-planning contexts.
- MCP's `next_task` must continue to skip human tasks, reflections, and any future personal-only records.

## 10. MVP scope

The first release should include:

1. Life areas and optional project-to-area association.
2. Journeys with milestones.
3. Task links to journey/milestone plus an optional scheduled date.
4. Today and This Week views.
5. Three weekly priorities and three daily MITs.
6. Daily/weekly reflections.
7. Simple daily/weekly habit check-ins.
8. Weekly review with intentional carry-forward actions.

The MVP does not require calendar sync, notifications, AI planning, recurring task automation, detailed analytics, or self-hosted GitLab support.

## 11. Suggested release phases

### Phase 1 — Planning foundation

Add life areas, journeys, milestones, task links, scheduling, and Today/This Week navigation. Preserve the existing board as the execution layer.

### Phase 2 — Review loop

Add weekly priorities, MITs, reflections, weekly review, and controlled carry-forward.

### Phase 3 — Habits and insight

Add habit check-ins, simple weekly completion summaries, and life-area balance signals.

### Phase 4 — Optional extensions

Evaluate recurring tasks, optional desktop reminders, calendar links, self-hosted GitLab support, and private agent-assisted planning only after real use validates the need.

## 12. Data and migration considerations

- Add new SQLite tables for `life_areas`, `journeys`, `milestones`, `weekly_priorities`, `habits`, `habit_checkins`, and `reflections`.
- Add nullable task fields for `life_area_id`, `journey_id`, `milestone_id`, `scheduled_for`, and `is_mit`/priority metadata as needed.
- Use additive migrations so existing Kanzo boards remain compatible.
- Keep reflections and habits in the same local synced database unless the user later requests a separate private-only store.
- Extend Markdown and JSON exports with life-area, journey, milestone, scheduling, habit, and reflection data.

## 13. Success measures

After four weeks of use, success looks like:

- The user plans at least three weeks using This Week.
- At least 70% of planned daily actions are either completed, consciously rescheduled, or deliberately dropped rather than silently forgotten.
- At least two active journeys have linked milestones and weekly priorities.
- The user completes at least two weekly reviews.
- Agent-managed technical work remains visible without overwhelming personal planning.

## 14. Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| The product becomes a complex all-in-one planner. | Ship the planning foundation first; defer integrations and analytics. |
| Existing coding workflow becomes harder to use. | Preserve Board as a first-class view and make personal planning additive. |
| Too many tasks create pressure. | Limit visible MITs and weekly priorities; keep habits separate from tasks. |
| Automatic rollovers create an inaccurate plan. | Require the user to decide what happens to unfinished work during review. |
| Agents interfere with personal planning. | Default reflections, habits, and personal actions to agent-excluded. |

## 15. Open decisions

1. Should a project belong to exactly one life area, or may it appear in multiple areas?
2. Should daily planning use only dates, or optional time blocks as well?
3. Should personal reflections sync through the existing Git repository by default?
4. Should habits count only completion, or optionally record a numeric value such as minutes read or distance run?
5. Should weekly planning start on Monday by default, with a user preference for Sunday?
6. Should a journey's progress be manual in MVP, or calculated from completed milestones?

## 16. Acceptance criteria for the MVP

- A user can create a life area, journey, and milestone without leaving Kanzo.
- A user can link a task to a journey/milestone and schedule it for today or a chosen week.
- Today clearly shows no more than three MITs, scheduled tasks, and habit check-ins.
- This Week clearly shows weekly priorities and unfinished work requiring a decision.
- A completed weekly review creates a durable reflection and next-week focus.
- Existing Kanzo boards open and operate normally after migration.
- Agents cannot select personal-only tasks, habits, or reflections through `next_task`.
- Board data still exports and syncs without storing credentials or requiring an account.

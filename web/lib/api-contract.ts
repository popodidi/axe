// Compile-time contract between the server and the dashboard: every API type
// generated from Rust (`lib/api.ts`) must satisfy the shape the dashboard
// reads. A renamed, removed or retyped field fails `npm run typecheck`; new
// server fields need no change here.
import type * as Api from "./api";
import type { DepRef, Event, Run, Task } from "./model";
import type {
  Activity,
  Bot,
  BotEvent,
  BotRun,
  BotSchedule,
  BotSummary,
  MessageMeta,
  TaskAction,
  Wake,
} from "./bots";
import type { Goal, GoalDetail, GoalEvent, GoalTask } from "./goals";
type Reads<Server extends Dashboard, Dashboard> = [Server, Dashboard];
/** `direction` is a string in Rust; the server only writes "in" or "out". */
type Meta = Omit<MessageMeta, "direction"> & { direction: string };
export type ApiContract = [
  Reads<Api.Task, Task>,
  Reads<Api.DepRef, DepRef>,
  Reads<Api.Event, Event>,
  Reads<Api.Run, Run>,
  Reads<Api.Bot, Bot>,
  Reads<Api.Bot, BotSummary>,
  Reads<Api.Wake, Wake>,
  Reads<Api.BotRun, BotRun>,
  Reads<Api.BotEvent, BotEvent>,
  Reads<Api.BotSchedule, BotSchedule>,
  Reads<Api.MessageMeta, Meta>,
  Reads<Api.TaskAction, TaskAction>,
  Reads<Api.Activity, Omit<Activity, "messages"> & { messages: Meta[] }>,
  Reads<Api.Goal, Goal>,
  Reads<Api.GoalEvent, GoalEvent>,
  Reads<Api.GoalTask, GoalTask>,
  Reads<Api.GoalDetail, GoalDetail>,
];

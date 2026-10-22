// The chat object's log events, in the JSON line format of design note 010: one flat object
// per event with `level`, `target`, `message`, a stable `event` name and the event's fields.
// Workers Logs indexes the fields of a logged object, so the object is logged, not a string.

export type Target = "chat" | "chat.room" | "chat.ctl" | "chat.echo" | "chat.notify";

/** Every event the chat object logs. design note 010 lists the fields of the room events. */
export type EventName =
  | "room_request"
  | "room_request_failed"
  | "room_queue_failed"
  | "ctl_request_failed"
  | "room_event"
  | "room_delivery"
  | "room_dial_failed"
  | "dial_back"
  | "bad_schedule_key"
  | "prune_schedule_failed"
  | "task_failed";

export type Fields = Readonly<Record<string, string | number | boolean>>;

export interface Logger {
  info(event: EventName, fields: Fields): void;
  warn(event: EventName, fields: Fields): void;
  error(event: EventName, fields: Fields): void;
}

/**
 * A logger for `target`. `endpoint` names the object's endpoint on every line; it is called
 * only when a line is written.
 */
export function logger(target: Target, endpoint: () => string): Logger {
  const line = (level: "info" | "warn" | "error", event: EventName, fields: Fields) => ({
    level,
    target,
    message: event,
    event,
    endpoint: endpoint(),
    ...fields,
  });
  return {
    info: (event, fields) => console.log(line("info", event, fields)),
    warn: (event, fields) => console.warn(line("warn", event, fields)),
    error: (event, fields) => console.error(line("error", event, fields)),
  };
}

/** The message of a thrown value, for a log field or a reply. */
export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Lets `task` run without awaiting it. Tasks handle the failures they expect, such as a peer
 * that goes away; anything else, a storage error or a bug, rejects the task and is logged
 * here with its stack.
 */
export function background(task: Promise<unknown>, log: Logger, name: string): void {
  task.catch((error: unknown) => {
    const stack = error instanceof Error && error.stack !== undefined ? error.stack : "";
    log.error("task_failed", { task: name, error: errorText(error), stack });
  });
}

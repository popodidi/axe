import { mockDesk, runs } from "./fixtures";
export const references = [
  "https://github.com/example/project/pull/779",
  "https://github.com/example/project/actions/runs/123456",
  "https://team.larksuite.com/base/synthetic?record=rec123",
  ...Array.from(
    { length: 18 },
    (_, i) =>
      `https://artifacts.example.com/synthetic/task-172/${"long-evidence-path/".repeat(4)}layout-${i}.png`,
  ),
  "https://unknown.example.com/" + "unrecognized/".repeat(20),
  "local evidence with no metadata",
];
export async function compactFixture(page: import("@playwright/test").Page) {
  await mockDesk(page);
  await page.route("http://axe.test/api/tasks/3/events", (route) =>
    route.fulfill({
      json: references.map((body, i) => ({
        id: i,
        kind: "artifact",
        body,
        at:
          i === references.length - 1
            ? ""
            : new Date(Date.UTC(2026, 10, 1, i)).toISOString(),
        actor: i === references.length - 1 ? "" : "synthetic-agent",
      })),
    }),
  );
  await page.route("http://axe.test/api/runs?*", (route) =>
    route.fulfill({
      json: [
        runs[0],
        {
          ...runs[1],
          task_id: 3,
          current_activity:
            "Finished process, task still awaiting review. " +
            "Full recorded result. ".repeat(30),
        },
      ],
    }),
  );
}

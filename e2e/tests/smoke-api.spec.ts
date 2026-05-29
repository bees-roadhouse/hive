import { expect, test } from "@playwright/test";

const apiBase = process.env.HIVE_API_URL ?? "http://127.0.0.1:7878";

test.describe("hive-api smoke", () => {
  test("healthz returns ok", async ({ request }) => {
    const res = await request.get(`${apiBase}/healthz`);
    expect(res.ok()).toBeTruthy();
    await expect(res.json()).resolves.toEqual({ ok: true });
  });

  test("inline journal tasks pipeline (PR branch API)", async ({ request }) => {
    const title = `playwright smoke ${Date.now()}`;
    const body = "- [ ] buy milk\n- [x] done thing ^task4\n- [ ] walk dog";

    const create = await request.post(`${apiBase}/journal`, {
      data: { ai: "pia", title, body, tags: "e2e-smoke" },
    });
    expect(create.ok()).toBeTruthy();
    const entry = await create.json();
    expect(entry.id).toBeTruthy();

    // Block ids should be assigned on write (feature branch hive-api).
    expect(entry.body).toMatch(/\^task\d+/);

    const tasksRes = await request.get(`${apiBase}/journal/${entry.id}/tasks`);
    test.skip(
      tasksRes.status() === 404,
      "running hive-api lacks GET /journal/{id}/tasks (rebuild from PR branch)",
    );
    expect(tasksRes.ok()).toBeTruthy();
    const tasks = await tasksRes.json();
    expect(tasks.length).toBeGreaterThanOrEqual(2);
    const titles = tasks.map((t: { title: string }) => t.title);
    expect(titles).toContain("buy milk");
    expect(titles).toContain("walk dog");
  });
});

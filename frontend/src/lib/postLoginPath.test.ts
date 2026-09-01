import { describe, expect, it } from "vitest";
import { consumePostLoginPath, rememberPostLoginPath } from "./postLoginPath";

describe("post-login destination", () => {
  it("is consumed exactly once", () => {
    rememberPostLoginPath("/settings/providers?account=new");

    expect(consumePostLoginPath()).toBe("/settings/providers?account=new");
    expect(consumePostLoginPath()).toBe("/");
  });

  it.each(["settings", "//outside.test/path"])("rejects a non-local path: %s", (path) => {
    expect(() => rememberPostLoginPath(path)).toThrow(/absolute path/);
  });

  it("fails fast when session storage contains a corrupt destination", () => {
    sessionStorage.setItem("flyco.post_login_path", "https://outside.test/path");

    expect(() => consumePostLoginPath()).toThrow(/absolute path/);
    expect(sessionStorage.getItem("flyco.post_login_path")).toBeNull();
  });
});

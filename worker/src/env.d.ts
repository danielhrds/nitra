// The Worker's bindings (see wrangler.jsonc). `cloudflare:workers` reads
// them from `Cloudflare.Env`; handlers use the global `Env`.

declare namespace Cloudflare {
  interface Env {
    ROOMS: DurableObjectNamespace<import("./room").Room>;
  }
  // Types `exports` from `cloudflare:workers` (used by the tests).
  interface GlobalProps {
    mainModule: typeof import("./index");
    durableNamespaces: "Room";
  }
}

interface Env extends Cloudflare.Env {}

import type { CliperBridge } from "../shared/bridge";

declare global {
  interface Window {
    cliper: CliperBridge;
  }
}

export {};

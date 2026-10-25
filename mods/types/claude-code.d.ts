/** Local stubs for the Claude Code plugin API. Not a copy of the bundled SDK. */

declare module "claude-code" {
    export type EngineInterface = {
        process: {
            run: (
                argv: readonly string[],
                options: { timeoutMs: number },
            ) => Promise<{
                exitCode: number;
                stdout: string;
                stderr: string;
                isStdoutTruncated: boolean;
                isStderrTruncated: boolean;
            }>;
        };
        store: {
            get: (key: string) => Promise<unknown>;
            set: (key: string, value: unknown) => Promise<void>;
        };
        session: {
            surfaces: () => Promise<readonly string[]>;
            start: (event: {
                cwd: string;
                surface: "terminal" | null;
                isInteractive: boolean;
            }) => Promise<void>;
        };
        ui: {
            toast: (text: string) => void;
            status: (text: string | undefined) => void;
        };
        command: {
            register: (command: { name: string; description: string }) => Promise<unknown>;
            run: (event: unknown) => Promise<{ text: string }>;
        };
        tool: {
            register: (tool: {
                name: string;
                description: string;
                isDeferred: boolean;
                inputSchema: unknown;
            }) => Promise<unknown>;
            call: (event: unknown) => Promise<{ result: unknown; text?: string }>;
        };
        clock: {
            every: (ms: number, tick: () => void) => { cancel: () => void };
        };
    };

    // The bundled Claude Code types are not in this repo. The stub is only
    // wide enough for `tsc`; plugin tests run against the real engine.
    export type On = {
        (event: string, ...rest: any[]): any;
    };

    export type Register = (on: On) => void;
}

declare module "claude-code/testing" {
    import type { EngineInterface, On } from "claude-code";

    export type Engine = EngineInterface & {
        session: EngineInterface["session"] & {
            start: EngineInterface["session"]["start"];
        };
    };

    export const test: (
        name: string,
        fn: (engine: Engine, on: On) => Promise<void> | void,
    ) => void;

    export const expect: {
        (value: unknown): {
            toEqual: (expected: unknown) => void;
            toBe: (expected: unknown) => void;
            toBeUndefined: () => void;
            not: { toContain: (expected: unknown) => void };
        };
    };

    export const mock: {
        clock: (on: On) => { advance: (ms: number) => Promise<void> };
        store: (on: On) => void;
    };
}

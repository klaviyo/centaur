import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";

import type { Skill } from "../api";
import { SkillForm } from "./SkillForm";

vi.mock("../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api")>()),
  useEditorCandidates: () => ({ data: [], isError: false }),
}));

const reader = { id: "usr_reader", name: "Reader", email: "reader@example.test" };
const skill: Skill = {
  id: "skl_private", name: "private-guide", description: "Private guidance", instructions: "# Guide",
  content_html: "<h1>Guide</h1>", visibility: "private", author: "Owner", owner: reader,
  updated_at: null, created_at: null, checksum: "checksum", lock_version: 3,
  editors: [], readers: [reader],
  viewer: { owner: true, can_edit: true, can_manage: true, can_unshare: false },
};

describe("skill reader controls", () => {
  it("preserves the owner's explicit reader grants when saving content", async () => {
    const save = vi.fn().mockResolvedValue(undefined);
    render(<MemoryRouter><SkillForm skill={skill} canManageEditors onSubmit={save} cancelTo="/" /></MemoryRouter>);
    expect(screen.getByRole("combobox", { name: "Readers" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Save skill" }));
    await waitFor(() => expect(save).toHaveBeenCalledWith(expect.objectContaining({ reader_ids: [reader.id], lock_version: 3 })));
  });

  it("does not expose reader management or submit reader grants for an editor", async () => {
    const save = vi.fn().mockResolvedValue(undefined);
    render(<MemoryRouter><SkillForm skill={{ ...skill, readers: [] }} canManageEditors={false} onSubmit={save} cancelTo="/" /></MemoryRouter>);
    expect(screen.queryByRole("combobox", { name: "Readers" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Save skill" }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    const submitted = save.mock.calls.at(0)?.[0];
    expect(submitted).toBeDefined();
    expect(submitted).not.toHaveProperty("reader_ids");
    expect(submitted).not.toHaveProperty("editor_ids");
  });
});

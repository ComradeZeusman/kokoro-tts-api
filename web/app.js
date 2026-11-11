"use strict";

(() => {
  const $ = (id) => document.getElementById(id);

  const state = {
    loginType: "customer",
    role: null,
    customer: null,
    usage: null,
    adminAccounts: [],
    selectedAccountId: null,
    ttsObjectUrl: null,
    combinedObjectUrl: null,
    voices: []
  };

  const views = ["login-view", "password-view", "customer-view", "admin-view"];

  function showView(viewId) {
    views.forEach((id) => {
      const element = $(id);
      if (element) element.classList.toggle("hidden", id !== viewId);
    });
  }

  function setHidden(id, hidden) {
    const element = $(id);
    if (element) element.classList.toggle("hidden", hidden);
  }

  function message(id, text, type = "") {
    const element = $(id);
    if (!element) return;
    element.textContent = text || "";
    element.classList.remove("error", "success");
    if (type) element.classList.add(type);
    element.classList.toggle("hidden", !text);
  }

  function setBusy(button, busy, busyText = "Please wait...") {
    if (!button) return;
    if (busy) {
      button.dataset.originalText = button.textContent.trim();
      button.dataset.wasDisabled = String(button.disabled);
      button.dataset.busy = "true";
      button.disabled = true;
      button.textContent = busyText;
    } else {
      button.disabled =
        button.dataset.accessDisabled === "true" ||
        button.dataset.wasDisabled === "true";

      button.dataset.busy = "false";
      delete button.dataset.wasDisabled;

      if (button.dataset.originalText) {
        button.textContent = button.dataset.originalText;
        delete button.dataset.originalText;
      }
    }
  }

  function formatNumber(value) {
    return Number(value || 0).toLocaleString();
  }

  function formatDate(timestamp) {
    if (!timestamp) return "Not available";
    const date = new Date(Number(timestamp) * 1000);
    return Number.isNaN(date.getTime())
      ? "Not available"
      : date.toLocaleDateString(undefined, {
          year: "numeric",
          month: "short",
          day: "numeric"
        });
  }

  function formatDuration(milliseconds) {
    const seconds = Math.max(0, Math.floor(Number(milliseconds || 0) / 1000));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const remainingSeconds = seconds % 60;

    if (hours) return `${hours}h ${minutes}m`;
    if (minutes) return `${minutes}m ${remainingSeconds}s`;
    return `${remainingSeconds}s`;
  }

  function accountStatusLabel(status) {
    const labels = {
      active: "Active",
      pending: "Pending activation",
      suspended: "Suspended"
    };
    return labels[status] || status || "Unknown";
  }

  async function request(path, options = {}) {
    const response = await fetch(path, {
      credentials: "same-origin",
      cache: "no-store",
      ...options
    });

    const raw = await response.text();
    let data = null;

    if (raw) {
      try {
        data = JSON.parse(raw);
      } catch {
        data = raw;
      }
    }

    if (!response.ok) {
      let detail = "The request could not be completed.";

      if (typeof data === "string" && data.trim()) {
        detail = data;
      } else if (data && typeof data === "object") {
        detail = data.error || data.message || data.detail || detail;
      }

      throw new Error(detail);
    }

    return data;
  }

  function postJson(path, payload) {
    return request(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload)
    });
  }

  function postEmpty(path) {
    return request(path, { method: "POST" });
  }

  async function requestAudio(path, payload) {
    const response = await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload)
    });

    if (!response.ok) {
      const raw = await response.text();
      let detail = raw || "Audio generation failed.";
      try {
        const parsed = JSON.parse(raw);
        if (typeof parsed === "string") detail = parsed;
        else detail = parsed.error || parsed.message || parsed.detail || detail;
      } catch {
        // Keep the original response text.
      }
      throw new Error(detail);
    }

    return response.blob();
  }

  function clearAudio(which) {
    const urlKey = which === "combined" ? "combinedObjectUrl" : "ttsObjectUrl";
    if (state[urlKey]) {
      URL.revokeObjectURL(state[urlKey]);
      state[urlKey] = null;
    }
  }

  function showAudio(blob, which, format) {
    const combined = which === "combined";
    const audio = $(combined ? "combined-audio" : "tts-audio");
    const download = $(combined ? "combined-download" : "tts-download");
    const result = $(combined ? "combined-audio-result" : "tts-result");

    if (!audio || !download || !result) return;

    clearAudio(which);

    const url = URL.createObjectURL(blob);
    state[combined ? "combinedObjectUrl" : "ttsObjectUrl"] = url;

    audio.src = url;
    download.href = url;
    download.download = `voicemw-${combined ? "combined" : "speech"}.${format}`;
    result.classList.remove("hidden");
  }

  function updateCharacterCount(inputId, outputId) {
    const input = $(inputId);
    const output = $(outputId);
    if (!input || !output) return;

    const count = input.value.length;

    if (inputId === "combined-transcript") {
      const remaining = 2000 - count;
      output.textContent = remaining >= 0
        ? `${formatNumber(count)} characters · ${formatNumber(remaining)} remaining for TTS`
        : `${formatNumber(count)} characters · ${formatNumber(Math.abs(remaining))} over TTS limit`;
      return;
    }

    output.textContent = `${formatNumber(count)} / 2000`;
  }

  function setTool(tool) {
    document.querySelectorAll("[data-tool]").forEach((button) => {
      button.classList.toggle("active", button.dataset.tool === tool);
    });

    ["tts", "stt", "combined"].forEach((name) => {
      const panel = $(`tool-${name}`);
      if (panel) panel.classList.toggle("hidden", name !== tool);
    });
  }

  function setLoginType(type) {
    state.loginType = type === "admin" ? "admin" : "customer";

    document.querySelectorAll("[data-login-type]").forEach((button) => {
      const selected = button.dataset.loginType === state.loginType;
      button.classList.toggle("active", selected);
      button.setAttribute("aria-selected", String(selected));
    });

    const heading = document.querySelector(".login-panel .panel-heading h2");
    if (heading) {
      heading.textContent = state.loginType === "admin"
        ? "Administrator sign in"
        : "Customer sign in";
    }

    message("login-message", "");
  }

  async function loadVoices() {
    try {
      const data = await request("/voices");
      state.voices = Array.isArray(data?.voices) ? data.voices : [];

      ["tts-voice", "combined-voice"].forEach((id) => {
        const select = $(id);
        if (!select) return;

        select.replaceChildren();

        state.voices.forEach((voice) => {
          const option = document.createElement("option");
          option.value = voice;
          option.textContent = voice;
          select.appendChild(option);
        });
      });
    } catch (error) {
      console.error("Could not load voices:", error.message);
      message("tts-message", "Voice options could not be loaded. Refresh the page and try again.", "error");
    }
  }

  async function loadCustomerData(me) {
    state.role = "customer";
    state.customer = me;

    if (me.must_change_password) {
      showView("password-view");
      message("password-message", "You must change your temporary password before using VoiceMW.");
      $("current-password")?.focus();
      return;
    }

    showView("customer-view");
    setHidden("logout-button", false);

    try {
      const [account, usage] = await Promise.all([
        request("/app/customer/account"),
        request("/app/customer/usage")
      ]);

      state.customer = { ...me, account };
      state.usage = usage;

      const summary = account?.account || me.account || {};
      if ($("customer-name")) {
        $("customer-name").textContent = summary.name || "Customer";
      }
      if ($("header-user")) {
        $("header-user").textContent = summary.name || summary.email || "Customer";
      }

      updateCustomerDashboard(usage, summary);
    } catch (error) {
      setCustomerToolsEnabled(false);
      message("customer-notice", error.message, "error");
    }
  }

  function updateCustomerDashboard(usage, account) {
    const status = usage?.account_status || account?.status || "pending";
    const subscription = usage?.subscription;

    if ($("customer-status")) {
      $("customer-status").textContent = accountStatusLabel(status);
    }

    if ($("subscription-status")) {
      $("subscription-status").textContent = subscription
        ? accountStatusLabel(subscription.status)
        : "No active subscription";
    }

    if ($("subscription-expiry")) {
      $("subscription-expiry").textContent = subscription
        ? formatDate(subscription.expires_at)
        : "Not available";
    }

    if ($("tts-remaining")) {
      $("tts-remaining").textContent = subscription
        ? formatNumber(subscription.characters_remaining)
        : "0";
    }

    if ($("stt-remaining")) {
      $("stt-remaining").textContent = subscription
        ? formatDuration(subscription.stt_remaining_ms)
        : "0s";
    }

    if ($("tts-usage-detail")) {
      $("tts-usage-detail").textContent = subscription
        ? `${formatNumber(subscription.characters_used)} of ${formatNumber(subscription.character_limit)} characters used`
        : "No subscription allowance available";
    }

    if ($("stt-usage-detail")) {
      $("stt-usage-detail").textContent = subscription
        ? `${formatDuration(subscription.stt_used_ms)} of ${formatDuration(subscription.stt_limit_ms)} used`
        : "No subscription allowance available";
    }

    const canUseTools = status === "active" && subscription?.status === "active";

    message(
      "customer-notice",
      canUseTools
        ? ""
        : status === "pending"
          ? "Your account is awaiting administrator activation. Your workspace will become available once your subscription is activated."
          : status === "suspended"
            ? "Your account is suspended. Please contact the VoiceMW administrator."
            : "Your account does not currently have an active subscription.",
      canUseTools ? "" : "error"
    );

    setCustomerToolsEnabled(canUseTools);
  }

  function setCustomerToolsEnabled(enabled) {
    ["tts-submit", "stt-submit", "combined-stt-submit", "combined-tts-submit"].forEach((id) => {
      const button = $(id);
      if (!button) return;

      button.dataset.accessDisabled = String(!enabled);

      if (button.dataset.busy !== "true") {
        button.disabled = !enabled;
      }
    });
  }

  async function refreshCustomerUsage() {
    try {
      const usage = await request("/app/customer/usage");
      state.usage = usage;
      updateCustomerDashboard(usage, state.customer?.account?.account || {});
    } catch (error) {
      setCustomerToolsEnabled(false);
      console.error("Usage refresh failed:", error.message);
    }
  }

  async function login(event) {
    event.preventDefault();

    const email = $("login-email")?.value.trim();
    const password = $("login-password")?.value;
    const type = state.loginType;
    const button = $("login-submit");

    if (!email || !password) {
      message("login-message", "Enter your email and password.", "error");
      return;
    }

    setBusy(button, true, "Signing in...");
    message("login-message", "");

    try {
      await postJson(`/auth/${type}/login`, { email, password });

      if (type === "customer") {
        const me = await request("/auth/customer/me");
        await loadCustomerData(me);
      } else {
        const me = await request("/auth/admin/me");
        await loadAdminWorkspace(me);
      }

      if ($("login-password")) $("login-password").value = "";
    } catch (error) {
      message("login-message", error.message, "error");
    } finally {
      setBusy(button, false);
    }
  }

  async function changePassword(event) {
    event.preventDefault();

    const current = $("current-password")?.value || "";
    const next = $("new-password")?.value || "";
    const confirm = $("confirm-password")?.value || "";

    if (next.length < 12) {
      message("password-message", "Your new password must contain at least 12 characters.", "error");
      return;
    }

    if (next !== confirm) {
      message("password-message", "The new passwords do not match.", "error");
      return;
    }

    const button = $("password-form")?.querySelector('[type="submit"]');
    setBusy(button, true, "Updating password...");
    message("password-message", "");

    try {
      const result = await postJson("/auth/customer/password", {
        current_password: current,
        new_password: next
      });

      $("password-form")?.reset();
      message("login-message", result?.message || "Password changed. Please sign in again.", "success");
      showView("login-view");
      setLoginType("customer");
      $("login-email")?.focus();
    } catch (error) {
      message("password-message", error.message, "error");
    } finally {
      setBusy(button, false);
    }
  }

  async function logout() {
    try {
      if (state.role === "admin") {
        await postEmpty("/auth/admin/logout");
      } else {
        await postEmpty("/auth/customer/logout");
      }
    } catch (error) {
      console.error("Logout request failed:", error.message);
    }

    state.role = null;
    state.customer = null;
    state.usage = null;
    state.selectedAccountId = null;

    if ($("login-form")) $("login-form").reset();
    if ($("password-form")) $("password-form").reset();

    setHidden("logout-button", true);
    showView("login-view");
    setLoginType("customer");
    message("login-message", "You have been signed out.", "success");
  }

  async function restoreSession() {
    try {
      const me = await request("/auth/customer/me");
      await loadCustomerData(me);
      setHidden("logout-button", false);
      return;
    } catch {
      // No valid customer session. Try the administrator session.
    }

    try {
      const me = await request("/auth/admin/me");
      await loadAdminWorkspace(me);
      setHidden("logout-button", false);
      return;
    } catch {
      // No existing session.
    }

    showView("login-view");
    setHidden("logout-button", true);
  }

  async function generateTts(textValue, voice, speed, format, messageId, button, combined = false) {
    const text = textValue.trim();

    if (!text) {
      message(messageId, "Enter some text before generating speech.", "error");
      return;
    }

    if (text.length > 2000) {
      message(messageId, "Text cannot exceed 2,000 characters.", "error");
      return;
    }

    setBusy(button, true, "Generating audio...");
    message(messageId, "Generating your audio...");
    setHidden(combined ? "combined-audio-result" : "tts-result", true);

    try {
      const blob = await requestAudio("/app/customer/tts", {
        text,
        voice: voice || undefined,
        speed: Number(speed || 1),
        format: format || "wav"
      });

      showAudio(blob, combined ? "combined" : "tts", format || "wav");
      message(messageId, "Your audio is ready.", "success");
      await refreshCustomerUsage();
    } catch (error) {
      message(messageId, error.message, "error");
    } finally {
      setBusy(button, false);
    }
  }

  async function submitTts(event) {
    event.preventDefault();

    await generateTts(
      $("tts-text")?.value || "",
      $("tts-voice")?.value,
      $("tts-speed")?.value,
      $("tts-format")?.value,
      "tts-message",
      $("tts-submit")
    );
  }

  async function transcribeFile(file, language, task, messageId, button, transcriptId, metaId) {
    if (!file) {
      message(messageId, "Choose an audio file first.", "error");
      return null;
    }

    if (file.size === 0) {
      message(messageId, "The selected file is empty.", "error");
      return null;
    }

    if (file.size > 25 * 1024 * 1024) {
      message(messageId, "The audio file exceeds the 25 MB upload limit.", "error");
      return null;
    }

    setBusy(button, true, "Transcribing...");
    message(messageId, "Uploading audio and transcribing...");
    setHidden("stt-progress", false);

    const form = new FormData();
    form.append("file", file);
    form.append("language", language.trim() || "auto");
    form.append("task", task || "transcribe");

    try {
      const result = await request("/app/customer/stt", {
        method: "POST",
        body: form
      });

      const transcript = result?.text;

      if (typeof transcript !== "string" || !transcript.trim()) {
        throw new Error("Transcription returned no text. Please try another audio file.");
      }

      if (transcriptId && $(transcriptId)) {
        $(transcriptId).value = transcript;
      }

      if (metaId && $(metaId)) {
        const duration = result?.duration
          ? `${Number(result.duration).toFixed(1)} seconds`
          : "Duration unavailable";
        $(metaId).textContent = `Language: ${result?.language || "Unknown"} · Audio: ${duration}`;
      }

      const resultPanel = transcriptId === "stt-transcript"
        ? "stt-result"
        : transcriptId === "combined-transcript"
          ? "combined-editor"
          : null;

      if (resultPanel) setHidden(resultPanel, false);

      message(messageId, "Transcription completed.", "success");
      await refreshCustomerUsage();
      return result;
    } catch (error) {
      message(messageId, error.message, "error");
      return null;
    } finally {
      setBusy(button, false);
      setHidden("stt-progress", true);
    }
  }

  async function submitStt(event) {
    event.preventDefault();

    await transcribeFile(
      $("stt-file")?.files?.[0],
      $("stt-language")?.value || "auto",
      $("stt-task")?.value || "transcribe",
      "stt-message",
      $("stt-submit"),
      "stt-transcript",
      "stt-result-meta"
    );
  }

  async function submitCombinedStt(event) {
    event.preventDefault();

    const result = await transcribeFile(
      $("combined-file")?.files?.[0],
      $("combined-language")?.value || "auto",
      $("combined-task")?.value || "transcribe",
      "combined-message",
      $("combined-stt-submit"),
      "combined-transcript",
      null
    );

    if (result && $("combined-auto")?.checked) {
      await generateTts(
        $("combined-transcript")?.value || "",
        $("combined-voice")?.value,
        $("combined-speed")?.value,
        $("combined-format")?.value,
        "combined-tts-message",
        $("combined-tts-submit"),
        true
      );
    }
  }

  async function submitCombinedTts() {
    await generateTts(
      $("combined-transcript")?.value || "",
      $("combined-voice")?.value,
      $("combined-speed")?.value,
      $("combined-format")?.value,
      "combined-tts-message",
      $("combined-tts-submit"),
      true
    );
  }

  async function copyText(value, messageId) {
    if (!value) {
      message(messageId, "There is nothing to copy.", "error");
      return;
    }

    try {
      await navigator.clipboard.writeText(value);
      message(messageId, "Copied to clipboard.", "success");
    } catch {
      message(messageId, "Clipboard access was denied by your browser.", "error");
    }
  }

  function createCell(value) {
    const cell = document.createElement("td");
    cell.textContent = value == null ? "" : String(value);
    return cell;
  }

  function renderAdminAccounts() {
    const body = $("admin-accounts-body");
    if (!body) return;

    body.replaceChildren();

    const query = ($("admin-search")?.value || "").trim().toLowerCase();

    const filtered = state.adminAccounts.filter((account) => {
      const searchable = [
        account.id,
        account.name,
        account.email,
        account.status
      ].join(" ").toLowerCase();

      return searchable.includes(query);
    });

    if ($("admin-account-count")) {
      $("admin-account-count").textContent =
        `${filtered.length} customer${filtered.length === 1 ? "" : "s"}`;
    }

    if (!filtered.length) {
      const row = document.createElement("tr");
      const cell = document.createElement("td");
      cell.colSpan = 6;
      cell.textContent = query ? "No matching customers found." : "No customers have been created yet.";
      row.appendChild(cell);
      body.appendChild(row);
      return;
    }

    filtered.forEach((account) => {
      const row = document.createElement("tr");

      row.appendChild(createCell(account.name));
      row.appendChild(createCell(account.email));
      row.appendChild(createCell(account.id));
      row.appendChild(createCell(accountStatusLabel(account.status)));
      row.appendChild(createCell(formatDate(account.created_at)));

      const actionCell = document.createElement("td");
      const button = document.createElement("button");
      button.type = "button";
      button.className = "button button-secondary";
      button.textContent = "View";
      button.dataset.accountId = account.id;
      button.addEventListener("click", () => openAdminAccount(account.id));
      actionCell.appendChild(button);
      row.appendChild(actionCell);

      body.appendChild(row);
    });
  }

  async function refreshAdminAccounts() {
    try {
      const accounts = await request("/app/admin/accounts");
      state.adminAccounts = Array.isArray(accounts) ? accounts : [];
      renderAdminAccounts();
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  async function loadAdminWorkspace(me) {
    state.role = "admin";
    state.customer = null;

    showView("admin-view");
    setHidden("logout-button", false);

    if ($("header-user")) {
      $("header-user").textContent = me.email || "Administrator";
    }

    await refreshAdminAccounts();
  }

  function renderAdminAccountDetails(details) {
    const account = details?.account || {};
    const subscription = details?.subscription;

    if ($("admin-detail-name")) {
      $("admin-detail-name").textContent = account.name || "Customer";
    }

    if ($("admin-detail-email")) {
      $("admin-detail-email").textContent = account.email || "";
    }

    if ($("admin-detail-summary")) {
      const lines = [
        `Account ID: ${account.id || "Unknown"}`,
        `Status: ${accountStatusLabel(account.status)}`,
        `Created: ${formatDate(account.created_at)}`,
        `Active API keys: ${formatNumber(details.active_api_keys)}`
      ];

      if (subscription) {
        lines.push(
          `Subscription: ${accountStatusLabel(subscription.status)}`,
          `Expires: ${formatDate(subscription.expires_at)}`,
          `TTS characters: ${formatNumber(subscription.characters_used)} / ${formatNumber(subscription.character_limit)}`,
          `STT audio: ${formatDuration(subscription.stt_used_ms)} / ${formatDuration(subscription.stt_limit_ms)}`
        );
      } else {
        lines.push("Subscription: None");
      }

      $("admin-detail-summary").textContent = lines.join("\n");
    }

    const suspended = account.status === "suspended";
    setHidden("admin-suspend", suspended);
    setHidden("admin-unsuspend", !suspended);

    if ($("admin-new-key")) $("admin-new-key").value = "";
    if ($("admin-new-key")) $("admin-new-key").type = "password";
    setHidden("admin-key-result", true);
  }

  async function openAdminAccount(accountId) {
    state.selectedAccountId = accountId;
    message("admin-message", "");

    try {
      const details = await request(
        `/app/admin/accounts/${encodeURIComponent(accountId)}`
      );

      renderAdminAccountDetails(details);
      setHidden("admin-detail", false);
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  function durationPayload(selectId, amountId) {
    const unit = $(selectId)?.value || "days";
    const amount = Number($(amountId)?.value);

    if (!Number.isInteger(amount) || amount < 1) {
      throw new Error("Enter a valid subscription duration.");
    }

    return unit.toLowerCase().includes("month")
      ? { months: amount }
      : { days: amount };
  }

  async function submitActivation(event) {
    event.preventDefault();

    if (!state.selectedAccountId) return;

    try {
      const duration = durationPayload("activate-duration", "activate-amount");
      const characters = Number($("activate-characters")?.value);
      const sttSeconds = Number($("activate-stt")?.value);

      if (!Number.isSafeInteger(characters) || characters < 1) {
        throw new Error("Enter a valid TTS character allowance.");
      }

      if (!Number.isSafeInteger(sttSeconds) || sttSeconds < 0) {
        throw new Error("Enter a valid STT allowance.");
      }

      const button = $("admin-activate-form")?.querySelector('[type="submit"]');
      setBusy(button, true, "Activating...");

      try {
        await postJson(
          `/app/admin/accounts/${encodeURIComponent(state.selectedAccountId)}/activate`,
          { ...duration, characters, stt_seconds: sttSeconds }
        );

        message("admin-message", "Subscription activated successfully.", "success");
        await refreshAdminAccounts();
        await openAdminAccount(state.selectedAccountId);
      } finally {
        setBusy(button, false);
      }
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  async function submitExtension(event) {
    event.preventDefault();

    if (!state.selectedAccountId) return;

    try {
      const duration = durationPayload("extend-duration", "extend-amount");
      const characters = Number($("extend-characters")?.value);
      const sttSeconds = Number($("extend-stt")?.value);

      if (!Number.isSafeInteger(characters) || characters < 0) {
        throw new Error("Enter a valid additional TTS allowance.");
      }

      if (!Number.isSafeInteger(sttSeconds) || sttSeconds < 0) {
        throw new Error("Enter a valid additional STT allowance.");
      }

      const button = $("admin-extend-form")?.querySelector('[type="submit"]');
      setBusy(button, true, "Extending...");

      try {
        await postJson(
          `/app/admin/accounts/${encodeURIComponent(state.selectedAccountId)}/extend`,
          {
            ...duration,
            add_characters: characters,
            add_stt_seconds: sttSeconds
          }
        );

        message("admin-message", "Subscription extended successfully.", "success");
        await refreshAdminAccounts();
        await openAdminAccount(state.selectedAccountId);
      } finally {
        setBusy(button, false);
      }
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  async function changeAccountStatus(action, confirmation, successText) {
    if (!state.selectedAccountId) return;
    if (!window.confirm(confirmation)) return;

    try {
      await postEmpty(
        `/app/admin/accounts/${encodeURIComponent(state.selectedAccountId)}/${action}`
      );

      message("admin-message", successText, "success");
      await refreshAdminAccounts();
      await openAdminAccount(state.selectedAccountId);
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  async function rotateApiKey() {
    if (!state.selectedAccountId) return;

    if (!window.confirm("Rotate this customer's API key? The existing key will stop working immediately.")) {
      return;
    }

    const button = $("admin-rotate-key");
    setBusy(button, true, "Rotating...");

    try {
      const result = await postEmpty(
        `/app/admin/accounts/${encodeURIComponent(state.selectedAccountId)}/keys/rotate`
      );

      if (!result?.api_key) {
        throw new Error("The server did not return the new API key.");
      }

      $("admin-new-key").value = result.api_key;
      $("admin-new-key").type = "text";
      setHidden("admin-key-result", false);

      message(
        "admin-message",
        "API key rotated. Copy the new key now; it will not be shown again.",
        "success"
      );

      await refreshAdminAccounts();
    } catch (error) {
      message("admin-message", error.message, "error");
    } finally {
      setBusy(button, false);
    }
  }

  async function revokeApiKeys() {
    if (!state.selectedAccountId) return;

    if (!window.confirm("Revoke every active API key for this customer? Existing integrations will stop working.")) {
      return;
    }

    try {
      const result = await postEmpty(
        `/app/admin/accounts/${encodeURIComponent(state.selectedAccountId)}/keys/revoke`
      );

      if ($("admin-new-key")) $("admin-new-key").value = "";
      setHidden("admin-key-result", true);

      message(
        "admin-message",
        `${formatNumber(result?.revoked_keys)} API key(s) revoked.`,
        "success"
      );

      await refreshAdminAccounts();
      await openAdminAccount(state.selectedAccountId);
    } catch (error) {
      message("admin-message", error.message, "error");
    }
  }

  function bindEvents() {
    $("login-form")?.addEventListener("submit", login);
    $("password-form")?.addEventListener("submit", changePassword);
    $("logout-button")?.addEventListener("click", logout);

    document.querySelectorAll("[data-login-type]").forEach((button) => {
      button.addEventListener("click", () => setLoginType(button.dataset.loginType));
    });

    document.querySelectorAll("[data-tool]").forEach((button) => {
      button.addEventListener("click", () => setTool(button.dataset.tool));
    });

    $("tts-form")?.addEventListener("submit", submitTts);
    $("stt-form")?.addEventListener("submit", submitStt);
    $("combined-stt-form")?.addEventListener("submit", submitCombinedStt);
    $("combined-tts-submit")?.addEventListener("click", submitCombinedTts);

    $("tts-text")?.addEventListener("input", () => {
      updateCharacterCount("tts-text", "tts-char-count");
    });

    $("combined-transcript")?.addEventListener("input", () => {
      updateCharacterCount("combined-transcript", "combined-char-count");
    });

    $("stt-copy")?.addEventListener("click", () => {
      copyText($("stt-transcript")?.value || "", "stt-message");
    });

    $("admin-search")?.addEventListener("input", renderAdminAccounts);
    $("admin-refresh")?.addEventListener("click", refreshAdminAccounts);

    $("admin-close-detail")?.addEventListener("click", () => {
      setHidden("admin-detail", true);
      state.selectedAccountId = null;
      if ($("admin-new-key")) $("admin-new-key").value = "";
      setHidden("admin-key-result", true);
    });

    $("admin-activate-form")?.addEventListener("submit", submitActivation);
    $("admin-extend-form")?.addEventListener("submit", submitExtension);

    $("admin-suspend")?.addEventListener("click", () => {
      changeAccountStatus(
        "suspend",
        "Suspend this customer account? Their customer workspace access will be blocked.",
        "Customer account suspended."
      );
    });

    $("admin-unsuspend")?.addEventListener("click", () => {
      changeAccountStatus(
        "unsuspend",
        "Unsuspend this customer account?",
        "Customer account unsuspended."
      );
    });

    $("admin-rotate-key")?.addEventListener("click", rotateApiKey);
    $("admin-revoke-keys")?.addEventListener("click", revokeApiKeys);

    $("admin-copy-key")?.addEventListener("click", async () => {
      const key = $("admin-new-key")?.value || "";
      if (!key) return;
      try {
        await navigator.clipboard.writeText(key);
        message("admin-message", "New API key copied. Store it securely.", "success");
      } catch {
        message("admin-message", "Clipboard access was denied. Select and copy the key manually.", "error");
      }
    });
  }

  async function init() {
    bindEvents();
    setLoginType("customer");
    setTool("tts");
    updateCharacterCount("tts-text", "tts-char-count");
    updateCharacterCount("combined-transcript", "combined-char-count");

    await loadVoices();
    await restoreSession();
  }

  document.addEventListener("DOMContentLoaded", init);
})();

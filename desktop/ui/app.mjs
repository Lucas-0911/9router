export function parseModelLines(value) {
  const models = value.split(/\r?\n/).map(item => item.trim()).filter(Boolean);
  if (!models.length || models.length > 100 || models.some(item => /\s/.test(item))) {
    throw new Error("Nhập từ 1 đến 100 model ID, mỗi dòng một mục, không có khoảng trắng.");
  }
  return [...new Set(models)];
}

export function parseAliases(value) {
  const aliases = Object.create(null);
  for (const line of value.split(/\r?\n/).filter(item => item.trim())) {
    const separator = line.indexOf("=");
    const name = line.slice(0, separator).trim();
    const target = line.slice(separator + 1).trim();
    if (separator < 1 || !/^[A-Za-z0-9_-]+$/.test(name) || !target || /\s/.test(target)) {
      throw new Error("Bí danh cần có dạng tên-ngắn = nhà-cung-cấp/model, không có khoảng trắng trong ID.");
    }
    if (Object.hasOwn(aliases, name)) throw new Error(`Bí danh bị trùng: ${name}`);
    aliases[name] = target;
  }
  return aliases;
}

export function connectionPayload(fields) {
  const baseUrl = fields.baseUrl.trim();
  if (!fields.provider || !fields.name.trim() || !fields.apiKey.trim()) {
    throw new Error("Điền nhà cung cấp, tên kết nối và API key.");
  }
  if (baseUrl) {
    const url = new URL(baseUrl);
    if (!/^https?:$/.test(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new Error("Base URL cần dùng HTTP/HTTPS và không chứa thông tin đăng nhập, truy vấn hay fragment.");
    }
  }
  const priority = Number(fields.priority);
  if (!Number.isSafeInteger(priority) || priority < 0 || priority > 100000) {
    throw new Error("Độ ưu tiên phải là số nguyên từ 0 đến 100000.");
  }
  return {
    provider: fields.provider, name: fields.name.trim(), apiKey: fields.apiKey.trim(),
    priority, isActive: true, providerSpecificData: baseUrl ? { baseUrl } : {},
  };
}

export function settingsPayload(value) {
  const settings = JSON.parse(value);
  if (!settings || Array.isArray(settings) || typeof settings !== "object") {
    throw new Error("Cài đặt phải là một đối tượng JSON.");
  }
  return settings;
}

export function isRunnableProvider(provider) {
  return ["apikey", "freeTier", "free"].includes(provider.category)
    && ["openai", "claude", "gemini", "openai-responses"].includes(provider.format)
    && provider.serviceKinds?.includes("llm")
    && provider.transport?.executor === "default"
    && !provider.transport.auth?.hooks?.length;
}

if (typeof document !== "undefined") start();

function start() {
  const $ = id => document.getElementById(id);
  const state = { config: null, page: "overview", providers: [], models: [], chat: null };
  const titles = { overview: "Tổng quan", connections: "Kết nối", models: "Mô hình", combos: "Combo", aliases: "Bí danh", keys: "Khóa API", chat: "Thử hội thoại", settings: "Cài đặt" };
  const text = (id, value) => { $(id).textContent = String(value ?? "—"); };
  const node = (tag, value, className) => {
    const result = document.createElement(tag);
    if (value !== undefined) result.textContent = String(value);
    if (className) result.className = className;
    return result;
  };
  function notice(message, error = false) {
    text("notice", message);
    $("notice").classList.toggle("error", error);
    $("notice").hidden = !message;
  }
  async function report(action) {
    try { return await action(); }
    catch (error) { notice(error.message || "Không thể hoàn tất thao tác.", true); }
  }
  async function api(path, { method = "GET", body, signal } = {}) {
    if (!state.config) throw new Error("Backend chưa sẵn sàng. Hãy mở giao diện từ ứng dụng desktop 9Router.");
    const controller = new AbortController();
    const timer = signal ? null : setTimeout(() => controller.abort(), 20000);
    try {
      const response = await fetch(state.config.backendUrl + path, {
        method, signal: signal || controller.signal, credentials: "omit", redirect: "error",
        headers: { Authorization: `Bearer ${state.config.adminToken}`, ...(body !== undefined ? { "Content-Type": "application/json" } : {}) },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      });
      const raw = await response.text();
      let result;
      try { result = raw ? JSON.parse(raw) : {}; }
      catch { throw new Error(`Backend trả về nội dung không hợp lệ (HTTP ${response.status}).`); }
      if (!response.ok) {
        const message = typeof result.error === "string" ? result.error : result.error?.message;
        throw new Error(message || `Yêu cầu thất bại (HTTP ${response.status}).`);
      }
      return result;
    } catch (error) {
      if (error.name === "AbortError") throw new Error(signal?.aborted ? "Đã hủy yêu cầu." : "Backend phản hồi quá chậm. Hãy thử lại.");
      if (error instanceof TypeError) throw new Error("Không kết nối được backend cục bộ. Hãy kiểm tra trạng thái ứng dụng.");
      throw error;
    } finally { clearTimeout(timer); }
  }
  async function deleteRecord(path, name, reload) {
    const dialog = $("delete-dialog");
    text("delete-description", `Xóa “${name}” khỏi cấu hình cục bộ? Thao tác này không thể hoàn tác.`);
    dialog.returnValue = "cancel";
    const confirmed = new Promise(resolve => dialog.addEventListener("close", () => resolve(dialog.returnValue === "delete"), { once: true }));
    dialog.showModal();
    if (!await confirmed) return;
    await api(path, { method: "DELETE" });
    await reload();
    notice("Đã xóa.");
  }
  function records(id, items, empty, render) {
    const target = $(id);
    target.replaceChildren();
    if (!items.length) target.append(node("p", empty, "empty"));
    for (const item of items) {
      const row = node("div", undefined, "record");
      render(item, row);
      target.append(row);
    }
  }
  function recordContent(row, title, detail, remove) {
    const content = node("div");
    content.append(node("h3", title), node("p", detail));
    row.append(content);
    if (remove) {
      const button = node("button", "Xóa", "secondary");
      button.type = "button";
      button.setAttribute("aria-label", `Xóa ${title}`);
      button.addEventListener("click", () => report(remove));
      row.append(button);
    }
  }
  async function loadStatus() {
    const status = await api("/api/status");
    text("version", status.version);
    text("health", "Backend đang chạy");
    text("runtime-state", "● Backend đang chạy");
    const implemented = status.implemented;
    const labels = { sqlite: "Lưu dữ liệu cục bộ", "model-resolution": "Chọn model", connections: "Quản lý kết nối", aliases: "Bí danh", combos: "Combo dự phòng", "api-keys": "Khóa cho ứng dụng", "generic-api-key-transports": "Kết nối bằng API key", "same-protocol-streaming": "Streaming cùng giao thức" };
    text("implemented-detail", Array.isArray(implemented) ? implemented.map(item => labels[item]).filter(Boolean).join(" · ") : "");
  }
  async function loadProviders() {
    const result = await api("/api/providers");
    state.providers = result.providers || [];
    const select = $("connection-provider");
    select.replaceChildren(new Option("Chọn nhà cung cấp API key", ""));
    for (const provider of state.providers.filter(isRunnableProvider)) {
      select.add(new Option(`${provider.name} · ${provider.format}`, provider.id));
    }
    records("provider-catalog", state.providers, "Chưa tải được danh mục.", (provider, row) => {
      row.className = "";
      row.append(node("span", provider.name), node("small", `${provider.id} · ${provider.category} · ${provider.format || "giao thức riêng"}`));
    });
  }
  async function loadConnections() {
    const { connections = [] } = await api("/api/providers/connections");
    text("connection-count", connections.length);
    text("connection-total", `${connections.length} kết nối`);
    records("connection-list", connections, "Chưa có kết nối. Thêm API key đầu tiên ở trên.", (item, row) => {
      recordContent(row, item.name || item.provider, `${item.provider} · ${item.isActive === false ? "Đã tắt" : "Đang bật"} · Ưu tiên ${item.priority ?? 0}${item.providerSpecificData?.baseUrl ? ` · ${item.providerSpecificData.baseUrl}` : ""}`, () => deleteRecord(`/api/providers/connections/${encodeURIComponent(item.id)}`, item.name || item.provider, loadConnections));
    });
  }
  function renderModels() {
    const query = $("model-search").value.toLocaleLowerCase();
    const filtered = state.models.filter(item => `${item.id} ${item.owned_by || ""}`.toLocaleLowerCase().includes(query));
    const table = $("model-list");
    table.replaceChildren();
    for (const item of filtered) {
      const row = node("tr");
      row.append(node("td", item.id), node("td", item.owned_by || "—"));
      table.append(row);
    }
    $("model-empty").hidden = Boolean(filtered.length);
  }
  async function loadModels() {
    const result = await api("/api/v1/models");
    state.models = result.data || [];
    text("model-count", state.models.length);
    renderModels();
    $("model-options").replaceChildren(...state.models.map(item => {
      const option = node("option");
      option.value = item.id;
      return option;
    }));
  }
  async function loadCombos() {
    const { combos = [] } = await api("/api/combos");
    text("combo-count", combos.length);
    records("combo-list", combos, "Chưa có combo. Thêm các model ID để tạo nhóm dự phòng.", (item, row) => {
      const models = (item.models || []).map(model => typeof model === "string" ? model : model.model || model.id || "?");
      recordContent(row, item.name, models.join(" → "), () => deleteRecord(`/api/combos/${encodeURIComponent(item.id)}`, item.name, loadCombos));
    });
  }
  async function loadAliases() {
    const result = await api("/api/models/alias");
    const aliases = result.aliases || result;
    $("alias-editor").value = Object.entries(aliases).map(([name, target]) => `${name} = ${typeof target === "string" ? target : `${target.provider}/${target.model}`}`).join("\n");
  }
  async function loadKeys() {
    const { apiKeys = [] } = await api("/api/keys");
    records("key-list", apiKeys, "Chưa có khóa. Tạo một khóa riêng cho mỗi ứng dụng.", (item, row) => {
      recordContent(row, item.name || "Khóa API", `${item.isActive === false ? "Đã tắt" : "Đang bật"} · ${item.id}`, () => deleteRecord(`/api/keys/${encodeURIComponent(item.id)}`, item.name || "Khóa API", loadKeys));
    });
  }
  async function loadSettings() {
    $("settings-editor").value = JSON.stringify(await api("/api/settings"), null, 2);
  }
  const loaders = {
    overview: () => Promise.all([loadStatus(), loadConnections(), loadModels(), loadCombos()]),
    connections: () => Promise.all([loadProviders(), loadConnections()]),
    models: () => Promise.all([loadProviders(), loadModels()]),
    combos: loadCombos, aliases: loadAliases, keys: loadKeys, settings: loadSettings, chat: loadModels,
  };
  async function refresh() {
    $("refresh").disabled = true;
    try { await loaders[state.page](); }
    finally { $("refresh").disabled = false; }
  }
  for (const button of document.querySelectorAll("[data-page]")) {
    button.addEventListener("click", () => {
      state.page = button.dataset.page;
      for (const panel of document.querySelectorAll("[data-panel]")) panel.hidden = panel.dataset.panel !== state.page;
      for (const item of document.querySelectorAll("[data-page]")) {
        if (item === button) item.setAttribute("aria-current", "page");
        else item.removeAttribute("aria-current");
      }
      text("page-title", titles[state.page]);
      notice("");
      $("main").focus();
      report(refresh);
    });
  }
  function onSubmit(id, action) {
    const form = $(id);
    form.addEventListener("submit", event => {
      event.preventDefault();
      report(async () => {
        const fields = Object.fromEntries(new FormData(form));
        const controls = [...form.querySelectorAll("input, select, textarea, button[type=submit]")];
        controls.forEach(control => { control.disabled = true; });
        form.setAttribute("aria-busy", "true");
        try { await action(fields, form); }
        finally {
          controls.forEach(control => { control.disabled = false; });
          form.removeAttribute("aria-busy");
        }
      });
    });
  }
  onSubmit("connection-form", async (fields, form) => {
    const payload = connectionPayload(fields);
    await api("/api/providers/connections", { method: "POST", body: payload });
    form.reset();
    await loadConnections();
    notice("Đã lưu kết nối. Dùng Thử hội thoại để kiểm tra API key.");
  });
  onSubmit("combo-form", async (fields, form) => {
    await api("/api/combos", { method: "POST", body: { name: fields.name.trim(), models: parseModelLines(fields.models) } });
    form.reset();
    await loadCombos();
    notice("Đã tạo combo.");
  });
  onSubmit("alias-form", async fields => {
    await api("/api/models/alias", { method: "PUT", body: parseAliases(fields.aliases) });
    notice("Đã lưu bí danh.");
  });
  onSubmit("key-form", async (fields, form) => {
    const result = await api("/api/keys", { method: "POST", body: { name: fields.name.trim() } });
    const key = typeof result.key === "string" ? result.key : typeof result.apiKey === "string" ? result.apiKey : result.apiKey?.key;
    if (key) {
      $("new-key-value").value = key;
      $("new-key").hidden = false;
    }
    form.reset();
    await loadKeys();
    notice(key ? "Đã tạo khóa API. Hãy sao chép khóa bên dưới." : "Đã tạo khóa API.");
  });
  onSubmit("settings-form", async fields => {
    await api("/api/settings", { method: "PUT", body: settingsPayload(fields.settings) });
    notice("Đã lưu cấu hình cục bộ.");
  });
  onSubmit("chat-form", async fields => {
    state.chat = new AbortController();
    $("chat-cancel").hidden = false;
    text("chat-state", "Đang đợi nhà cung cấp…");
    text("chat-output", "Đang gửi yêu cầu…");
    text("chat-usage", "");
    const started = performance.now();
    const timer = setTimeout(() => state.chat?.abort(), 120000);
    try {
      const result = await api("/api/v1/chat/completions", { method: "POST", signal: state.chat.signal, body: { model: fields.model.trim(), messages: [{ role: "user", content: fields.prompt }], stream: false } });
      const content = result.choices?.[0]?.message?.content;
      text("chat-output", typeof content === "string" ? content : JSON.stringify(result, null, 2));
      const usage = result.usage;
      text("chat-usage", `${((performance.now() - started) / 1000).toFixed(1)} giây${usage ? ` · ${usage.total_tokens ?? "—"} token` : ""}`);
      text("chat-state", "Đã nhận phản hồi.");
    } catch (error) {
      text("chat-state", "Yêu cầu chưa hoàn tất.");
      text("chat-output", error.message);
      throw error;
    } finally {
      clearTimeout(timer);
      state.chat = null;
      $("chat-cancel").hidden = true;
    }
  });
  async function copy(value) {
    await navigator.clipboard.writeText(value);
    notice("Đã sao chép vào clipboard.");
  }
  $("copy-endpoint").addEventListener("click", () => report(() => {
    if (!state.config) throw new Error("Backend chưa sẵn sàng.");
    return copy(`${state.config.backendUrl}/v1`);
  }));
  $("copy-key").addEventListener("click", () => report(() => copy($("new-key-value").value)));
  $("dismiss-key").addEventListener("click", () => { $("new-key-value").value = ""; $("new-key").hidden = true; });
  $("chat-cancel").addEventListener("click", () => state.chat?.abort());
  $("refresh").addEventListener("click", () => report(refresh));
  $("model-search").addEventListener("input", renderModels);
  report(async () => {
    if (!window.__TAURI__?.core?.invoke) {
      text("runtime-state", "○ Chưa kết nối backend");
      text("health", "Cần mở ứng dụng desktop");
      throw new Error("Hãy mở 9Router desktop để kết nối backend. Giao diện không lấy khóa quản trị từ URL hay bộ nhớ trình duyệt.");
    }
    state.config = await window.__TAURI__.core.invoke("backend_config");
    text("endpoint", `${state.config.backendUrl}/v1`);
    try { await refresh(); }
    catch (error) {
      text("runtime-state", "○ Không kết nối được backend");
      text("health", "Không kết nối được");
      throw error;
    }
  });
}

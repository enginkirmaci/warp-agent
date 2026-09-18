using System.Net.Http;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using WarpAgent.Models;

namespace WarpAgent.Services;

public sealed record SessionInfo(string Id, string Title, string? Directory, DateTime UpdatedAt);

public sealed record StoredMessage(string Id, string Role, long CreatedMs, JsonElement Info, List<JsonElement> Parts);

public sealed record ModelInfo(string Key, IReadOnlyList<string> Variants, int ContextLimit);

public sealed record AgentInfo(string Name, string Description, string Mode, bool Hidden);

/// <summary>
/// REST + SSE calls against an opencode server (v1 surface verified live against
/// 1.18.31). The chat is driven purely by the /event stream: the message POST is
/// fire-and-forget, assistant output arrives as message.part.updated events whose
/// "part" carries the full text-so-far each time.
/// </summary>
public sealed class OpenCodeApiClient(string baseUrl)
{
    private static readonly HttpClient Http = new() { Timeout = Timeout.InfiniteTimeSpan };
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    public string BaseUrl { get; } = baseUrl.TrimEnd('/');

    public async Task<SessionInfo> CreateSessionAsync(string title, CancellationToken ct)
    {
        using var resp = await PostAsync("/session", new { title }, TimeSpan.FromSeconds(30), ct);
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        return ReadSession(doc.RootElement);
    }

    public async Task<List<SessionInfo>> ListSessionsAsync(CancellationToken ct)
    {
        using var resp = await Http.GetAsync($"{BaseUrl}/session", ct);
        resp.EnsureSuccessStatusCode();
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        var sessions = new List<SessionInfo>();
        if (doc.RootElement.ValueKind == JsonValueKind.Array)
            foreach (var s in doc.RootElement.EnumerateArray())
                sessions.Add(ReadSession(s));
        return sessions;
    }

    public async Task<List<StoredMessage>> GetMessagesAsync(string sessionId, CancellationToken ct)
    {
        using var resp = await Http.GetAsync($"{BaseUrl}/session/{sessionId}/message", ct);
        resp.EnsureSuccessStatusCode();
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        var messages = new List<StoredMessage>();
        if (doc.RootElement.ValueKind != JsonValueKind.Array)
            return messages;
        foreach (var m in doc.RootElement.EnumerateArray())
        {
            var info = m.TryGetProperty("info", out var i) ? i : default;
            var id = info.TryGetProperty("id", out var mid) ? mid.GetString() : null;
            var role = info.TryGetProperty("role", out var r) ? r.GetString() : null;
            if (id is null || role is null)
                continue;
            var created = info.TryGetProperty("time", out var t) &&
                          t.TryGetProperty("created", out var c) &&
                          c.TryGetInt64(out var ms) ? ms : 0;
            var parts = new List<JsonElement>();
            if (m.TryGetProperty("parts", out var ps) && ps.ValueKind == JsonValueKind.Array)
                parts.AddRange(ps.EnumerateArray().Select(p => p.Clone()));
            messages.Add(new StoredMessage(id, role, created, info.Clone(), parts));
        }
        return messages;
    }

    public async Task<SessionInfo> RenameSessionAsync(string sessionId, string title, CancellationToken ct)
    {
        using var resp = await SendAsync(HttpMethod.Patch, $"/session/{sessionId}", new { title }, TimeSpan.FromSeconds(15), ct);
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        return ReadSession(doc.RootElement);
    }

    public Task DeleteSessionAsync(string sessionId, CancellationToken ct) =>
        SendAsync(HttpMethod.Delete, $"/session/{sessionId}", null, TimeSpan.FromSeconds(15), ct);

    public Task AbortAsync(string sessionId, CancellationToken ct) =>
        PostAsync($"/session/{sessionId}/abort", new { }, TimeSpan.FromSeconds(15), ct);

    public Task SendMessageAsync(string sessionId, string text, string? model, string? variant, string? agent, IReadOnlyList<FileAttachment> attachments, CancellationToken ct)
    {
        var parts = new List<object>();
        if (text.Length > 0)
            parts.Add(new { type = "text", text });
        foreach (var attachment in attachments)
            parts.Add(new
            {
                type = "file",
                mime = attachment.Mime,
                filename = attachment.FileName,
                url = attachment.DataUrl,
            });
        var body = new Dictionary<string, object> { ["parts"] = parts };
        if (model is not null)
            body["model"] = SplitModel(model);
        if (variant is not null)
            body["variant"] = variant;
        if (agent is not null)
            body["agent"] = agent;
        return PostAsync($"/session/{sessionId}/message", body, Timeout.InfiniteTimeSpan, ct);
    }

    public Task RespondPermissionAsync(string sessionId, string permissionId, string response, CancellationToken ct) =>
        PostAsync($"/session/{sessionId}/permissions/{permissionId}", new { response }, TimeSpan.FromSeconds(15), ct);

    public Task ReplyV2Async(string requestId, string reply, CancellationToken ct) =>
        PostAsync($"/permission/{requestId}/reply", new { reply }, TimeSpan.FromSeconds(15), ct);

    /// <summary>Shares the session; returns the public URL, or null when already shared without a URL.</summary>
    public async Task<string?> ShareAsync(string sessionId, CancellationToken ct)
    {
        using var resp = await PostAsync($"/session/{sessionId}/share", new { }, TimeSpan.FromSeconds(30), ct);
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        return doc.RootElement.TryGetProperty("share", out var share) &&
               share.TryGetProperty("url", out var url)
            ? url.GetString()
            : null;
    }

    public Task UnshareAsync(string sessionId, CancellationToken ct) =>
        SendAsync(HttpMethod.Delete, $"/session/{sessionId}/share", null, TimeSpan.FromSeconds(30), ct);

    public async Task<List<AgentInfo>> GetAgentsAsync(CancellationToken ct)
    {
        using var resp = await Http.GetAsync($"{BaseUrl}/agent", ct);
        resp.EnsureSuccessStatusCode();
        using var doc = await JsonDocument.ParseAsync(await resp.Content.ReadAsStreamAsync(ct), cancellationToken: ct);
        var agents = new List<AgentInfo>();
        if (doc.RootElement.ValueKind != JsonValueKind.Array)
            return agents;
        foreach (var a in doc.RootElement.EnumerateArray())
        {
            var name = a.TryGetProperty("name", out var n) ? n.GetString() : null;
            if (name is null)
                continue;
            agents.Add(new AgentInfo(
                name,
                a.TryGetProperty("description", out var d) ? d.GetString() ?? "" : "",
                a.TryGetProperty("mode", out var m) ? m.GetString() ?? "" : "",
                a.TryGetProperty("hidden", out var h) && h.ValueKind == JsonValueKind.True));
        }
        return agents;
    }

    public async Task<List<ModelInfo>> GetModelsAsync(CancellationToken ct)
    {
        using var resp = await Http.GetAsync($"{BaseUrl}/config/providers", ct);
        resp.EnsureSuccessStatusCode();
        using var doc = JsonDocument.Parse(await resp.Content.ReadAsStringAsync(ct));
        var models = new List<ModelInfo>();
        if (doc.RootElement.TryGetProperty("providers", out var providers) && providers.ValueKind == JsonValueKind.Array)
        {
            foreach (var provider in providers.EnumerateArray())
            {
                var pid = provider.TryGetProperty("id", out var pi) ? pi.GetString() : null;
                if (pid is null || !provider.TryGetProperty("models", out var modelsMap) ||
                    modelsMap.ValueKind != JsonValueKind.Object)
                    continue;
                foreach (var m in modelsMap.EnumerateObject())
                {
                    var variants = new List<string>();
                    if (m.Value.TryGetProperty("variants", out var v) && v.ValueKind == JsonValueKind.Object)
                        foreach (var name in v.EnumerateObject())
                            variants.Add(name.Name);
                    variants.Sort(StringComparer.OrdinalIgnoreCase);
                    var contextLimit = m.Value.TryGetProperty("limit", out var lim) &&
                                       lim.TryGetProperty("context", out var ctx) &&
                                       ctx.TryGetInt32(out var limit)
                        ? limit
                        : 0;
                    models.Add(new ModelInfo($"{pid}/{m.Name}", variants, contextLimit));
                }
            }
        }
        return models;
    }

    public async IAsyncEnumerable<JsonElement> StreamEventsAsync([EnumeratorCancellation] CancellationToken ct)
    {
        using var req = new HttpRequestMessage(HttpMethod.Get, $"{BaseUrl}/event");
        using var resp = await Http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, ct);
        resp.EnsureSuccessStatusCode();
        await using var stream = await resp.Content.ReadAsStreamAsync(ct);
        var reader = new StreamReader(stream);

        while (await reader.ReadLineAsync(ct) is { } line)
        {
            if (!line.StartsWith("data:", StringComparison.Ordinal))
                continue;
            var payload = line["data:".Length..].Trim();
            if (payload.Length == 0)
                continue;
            JsonElement evt;
            try
            {
                using var doc = JsonDocument.Parse(payload);
                evt = doc.RootElement.Clone();
            }
            catch (JsonException)
            {
                continue;
            }
            yield return evt;
        }
    }

    private static SessionInfo ReadSession(JsonElement s)
    {
        var id = s.TryGetProperty("id", out var i) ? i.GetString() ?? "" : "";
        var title = s.TryGetProperty("title", out var t) ? t.GetString() ?? "" : "";
        var directory = s.TryGetProperty("directory", out var d) ? d.GetString() : null;
        var updated = s.TryGetProperty("time", out var time) &&
                      time.TryGetProperty("updated", out var u) &&
                      u.TryGetInt64(out var ms) ? FromMs(ms) : DateTime.MinValue;
        return new SessionInfo(id, title, directory, updated);
    }

    private static DateTime FromMs(long ms) => DateTimeOffset.FromUnixTimeMilliseconds(ms).ToLocalTime().DateTime;

    // a ValueTuple would serialize as an ARRAY; the API needs the object shape
    private static object SplitModel(string model)
    {
        var split = model.Split('/', 2);
        return split.Length == 2
            ? new { providerID = split[0], modelID = split[1] }
            : new { providerID = model, modelID = "" };
    }

    private async Task<HttpResponseMessage> PostAsync(string path, object body, TimeSpan timeout, CancellationToken ct) =>
        await SendAsync(HttpMethod.Post, path, body, timeout, ct);

    private async Task<HttpResponseMessage> SendAsync(HttpMethod method, string path, object? body, TimeSpan timeout, CancellationToken ct)
    {
        using var timeoutCts = timeout == Timeout.InfiniteTimeSpan
            ? null
            : CancellationTokenSource.CreateLinkedTokenSource(ct);
        timeoutCts?.CancelAfter(timeout);
        using var req = new HttpRequestMessage(method, BaseUrl + path);
        if (body is not null)
            req.Content = new StringContent(JsonSerializer.Serialize(body, JsonOptions), Encoding.UTF8, "application/json");
        var resp = await Http.SendAsync(req, timeoutCts?.Token ?? ct);
        if (!resp.IsSuccessStatusCode)
        {
            var detail = Truncate(await resp.Content.ReadAsStringAsync(ct), 1000);
            throw new HttpRequestException($"{(int)resp.StatusCode} {resp.ReasonPhrase}{(detail.Length > 0 ? $" — {detail}" : "")}");
        }
        return resp;
    }

    private static string Truncate(string s, int max) => s.Length <= max ? s : s[..max] + "…";
}

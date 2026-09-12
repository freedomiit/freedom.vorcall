using System.Text.Json;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class LoggingTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task A_session_writes_its_identity_into_the_days_json_log()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        // The file sink flushes on an interval, so the line is awaited rather than expected at once.
        JsonElement? line = null;
        var deadline = DateTime.UtcNow.AddSeconds(6);
        while (line is null && DateTime.UtcNow < deadline)
        {
            line = FindSessionLine(alice);
            if (line is null)
            {
                await Task.Delay(200);
            }
        }

        Assert.True(line is not null, $"no line of {Server.LogsDir} carries the session of {alice}");
        var found = line.Value;
        Assert.Equal(alice.UserId, found.GetProperty("UserId").GetInt64());
        Assert.Equal(alice.Username, found.GetProperty("Username").GetString());
        Assert.NotEmpty(found.GetProperty("SessionId").GetString() ?? string.Empty);
        Assert.True(found.TryGetProperty("@t", out _), "not a compact JSON event");
    }

    private JsonElement? FindSessionLine(Account account)
    {
        foreach (var path in Directory.EnumerateFiles(Server.LogsDir, "vorcall-*.json"))
        {
            // The sink holds the file open for writing; the last line may still be half written.
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
            using var reader = new StreamReader(stream);
            while (reader.ReadLine() is { } text)
            {
                if (!text.Contains("\"SessionId\"", StringComparison.Ordinal) || !text.Contains(account.Username, StringComparison.Ordinal))
                {
                    continue;
                }

                JsonElement root;
                try
                {
                    using var document = JsonDocument.Parse(text);
                    root = document.RootElement.Clone();
                }
                catch (JsonException)
                {
                    continue;
                }

                if (root.TryGetProperty("UserId", out var userId)
                    && userId.ValueKind == JsonValueKind.Number
                    && userId.GetInt64() == account.UserId
                    && root.TryGetProperty("Username", out _))
                {
                    return root;
                }
            }
        }

        return null;
    }
}

using System.Net;
using Vorcall.Server.Protocol;
using Xunit;

namespace Vorcall.Server.Tests.Infrastructure;

// GET /api/messages. The query values are strings on purpose: half of what the endpoint's
// tests prove is how it answers a value that is not a number, and a channel id is a number now.
internal static class History
{
    public static Task<ProtoResponse> RawAsync(
        VorcallFactory factory,
        string bearer,
        string? channel = null,
        string? limit = null,
        string? before = null)
    {
        var query = new List<string>();
        if (channel is not null)
        {
            query.Add($"channel={Uri.EscapeDataString(channel)}");
        }

        if (limit is not null)
        {
            query.Add($"limit={Uri.EscapeDataString(limit)}");
        }

        if (before is not null)
        {
            query.Add($"before={Uri.EscapeDataString(before)}");
        }

        var path = "/api/messages" + (query.Count > 0 ? "?" + string.Join('&', query) : string.Empty);
        return Proto.GetAsync(factory, path, bearer);
    }

    public static async Task<MessagePage> PageAsync(
        VorcallFactory factory,
        string bearer,
        long channelId,
        string? limit = null,
        string? before = null)
    {
        var response = await RawAsync(factory, bearer, Id(channelId), limit, before);
        Assert.True(response.Status == HttpStatusCode.OK, $"messages: {(int)response.Status}");
        Assert.Contains(Proto.ContentType, response.Header("Content-Type") ?? string.Empty);
        return response.As(MessagePage.Parser);
    }

    // The page's messages by id, for the tests that look one message up after a change.
    public static async Task<Dictionary<long, ChatMessage>> ByIdAsync(VorcallFactory factory, string bearer, long channelId)
    {
        var page = await PageAsync(factory, bearer, channelId);
        return page.Messages.ToDictionary(message => message.Id);
    }

    // A channel id as the query string carries it: digits only, which is all the endpoint accepts.
    public static string Id(long channelId) => channelId.ToString(System.Globalization.CultureInfo.InvariantCulture);
}

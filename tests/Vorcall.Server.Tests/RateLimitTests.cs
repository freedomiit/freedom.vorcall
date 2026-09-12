using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class RateLimitTests(ServerFixture fixture)
{
    [Fact]
    public async Task The_write_frame_past_the_burst_answers_non_fatal_RATE_LIMITED_and_the_socket_stays_open()
    {
        var server = await fixture.TightLimitsAsync();
        var alice = await Accounts.RegisterAsync(server, "alice");
        await using var a1 = await WsClient.ConnectAsync(server, alice);

        long lastId = 0;
        for (var i = 0; i < ServerFixture.TightMessageBurst; i++)
        {
            await a1.SendAsync(Frames.Send($"burst {i}"));
            lastId = (await a1.ExpectAsync(Kind.Message)).Message.Id;
        }

        await a1.SendAsync(Frames.Send("one too many"));
        var refused = await a1.ExpectErrorAsync(ErrorCode.RateLimited, fatal: false);
        Assert.Equal("too many messages, slow down", refused.Detail);

        // Reads and presence stay free: MarkRead goes through, and the pong proves the socket
        // is still being served in order.
        await a1.SendAsync(Frames.MarkRead(Session.General, lastId));
        await a1.PingFenceAsync(lastId);

        await a1.SendAsync(Frames.Send("still refused"));
        await a1.ExpectErrorAsync(ErrorCode.RateLimited, fatal: false);
        await a1.PingFenceAsync(lastId + 1);
    }
}

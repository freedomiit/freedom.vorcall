using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class MessagesTests(ServerFixture fixture)
{
    // Far past any serial id this suite's database will reach.
    private const long UnknownId = 9_000_000;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Send_broadcasts_to_every_member_including_the_sender_under_one_id()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1, c1) = (party.Account(0), party.Client(0), party.Client(1));
        var general = party.GeneralId;

        var text = $"hello general {Frames.NowMs()}";
        await a1.SendAsync(Frames.Send(text, general));
        var echo = (await a1.ExpectAsync(Kind.Message)).Message;
        var mirror = (await c1.ExpectAsync(Kind.Message)).Message;
        foreach (var message in new[] { echo, mirror })
        {
            Assert.Equal(text, message.Text);
            Assert.Equal(general, message.ChannelId);
            Assert.Equal(alice.Username, message.Author);
            Assert.Equal(alice.UserId, message.AuthorId);
            Assert.True(message.Id > 0);
            Assert.InRange(Math.Abs(message.SentAtUnixMs - Frames.NowMs()), 0, 10_000);
        }

        Assert.Equal(echo.Id, mirror.Id);
    }

    [Fact]
    public async Task Send_without_a_channel_or_to_a_voice_channel_answers_non_fatally()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (a1, c1) = (party.Client(0), party.Client(1));

        // 0 is the wire's "absent" and no longer means general: a channel id is required, and an
        // unknown one is answered the same way as one the sender may not view.
        foreach (var channelId in new[] { 0L, UnknownId })
        {
            await a1.SendAsync(Frames.Send("into the void", channelId));
            await a1.ExpectErrorAsync(ErrorCode.UnknownChannel, fatal: false);
        }

        // A voice channel exists and is visible; it simply holds no messages.
        await a1.SendAsync(Frames.Send("talking into the mic", party.GeneralVoiceId));
        Assert.Equal("channel", (await a1.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false)).Detail);

        await c1.QuietAsync();
    }

    [Fact]
    public async Task Send_with_blank_or_oversized_text_answers_INVALID_MESSAGE_and_broadcasts_nothing()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var a1 = party.Client(0);
        foreach (var bad in new[] { "   ", new string('x', 2001) })
        {
            await a1.SendAsync(Frames.Send(bad, party.GeneralId));
            await a1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);
        }

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task Send_accepts_2000_characters_and_trims_the_text()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        await party.Client(0).SendAsync(Frames.Send("  " + new string('y', 2000) + "  ", party.GeneralId));
        var echo = (await party.Client(0).ExpectAsync(Kind.Message)).Message;
        Assert.Equal(2000, echo.Text.Length);
        await party.Client(1).ExpectAsync(Kind.Message);
    }

    [Fact]
    public async Task Ping_echoes_its_timestamp_in_a_Pong()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        await a1.SendAsync(Frames.Ping(123456789));
        var pong = (await a1.ExpectAsync(Kind.Pong)).Pong;
        Assert.Equal(123456789L, pong.SentAtUnixMs);
    }
}

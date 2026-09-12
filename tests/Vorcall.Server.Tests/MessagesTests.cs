using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class MessagesTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Send_broadcasts_to_every_member_including_the_sender_under_one_id()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1, c1) = (party.Account(0), party.Client(0), party.Client(1));

        var text = $"hello general {Frames.NowMs()}";
        await a1.SendAsync(Frames.Send(text, Session.General));
        var echo = (await a1.ExpectAsync(Kind.Message)).Message;
        var mirror = (await c1.ExpectAsync(Kind.Message)).Message;
        foreach (var message in new[] { echo, mirror })
        {
            Assert.Equal(text, message.Text);
            Assert.Equal(Session.General, message.RoomId);
            Assert.Equal(alice.Username, message.Author);
            Assert.Equal(alice.UserId, message.AuthorId);
            Assert.True(message.Id > 0);
            Assert.InRange(Math.Abs(message.SentAtUnixMs - Frames.NowMs()), 0, 10_000);
        }

        Assert.Equal(echo.Id, mirror.Id);

        // An empty room id means general.
        var implicitText = $"empty room id {Frames.NowMs()}";
        await a1.SendAsync(Frames.Send(implicitText));
        var echoed = (await a1.ExpectAsync(Kind.Message)).Message;
        Assert.Equal(Session.General, echoed.RoomId);
        Assert.Equal(implicitText, echoed.Text);
        await c1.ExpectAsync(Kind.Message);
    }

    [Fact]
    public async Task Send_to_an_unknown_or_invalid_room_answers_non_fatal_UNKNOWN_ROOM()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);
        foreach (var room in new[] { "nope", "Bad!" })
        {
            await a1.SendAsync(Frames.Send("into the void", room));
            await a1.ExpectErrorAsync(ErrorCode.UnknownRoom, fatal: false);
        }
    }

    [Fact]
    public async Task Send_with_blank_or_oversized_text_answers_INVALID_MESSAGE_and_broadcasts_nothing()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var a1 = party.Client(0);
        foreach (var bad in new[] { "   ", new string('x', 2001) })
        {
            await a1.SendAsync(Frames.Send(bad));
            await a1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);
        }

        await party.Client(1).QuietAsync();
    }

    [Fact]
    public async Task Send_accepts_2000_characters_and_trims_the_text()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        await party.Client(0).SendAsync(Frames.Send("  " + new string('y', 2000) + "  "));
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

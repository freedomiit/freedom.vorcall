using System.Diagnostics;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class ProtocolTests(ServerFixture fixture)
{
    private const int PolicyViolation = 1008;

    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Hello_with_an_unsupported_version_answers_fatal_PROTOCOL_and_closes_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.OpenAsync(Server, alice.Access, "alice/v2");
        await ws.SendAsync(Frames.Hello(protocolVersion: 2));
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true);
        Assert.Equal(PolicyViolation, await ws.ExpectClosedAsync(PolicyViolation));
    }

    [Fact]
    public async Task A_frame_before_hello_answers_fatal_PROTOCOL_and_closes_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.OpenAsync(Server, alice.Access, "alice/early");
        await ws.SendAsync(Frames.Send("too early"));
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true);
        await ws.ExpectClosedAsync(PolicyViolation);
    }

    [Fact]
    public async Task A_text_frame_answers_fatal_PROTOCOL_and_closes_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.OpenAsync(Server, alice.Access, "alice/text");
        await ws.SendTextAsync("hello as text");
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true);
        await ws.ExpectClosedAsync(PolicyViolation);
    }

    [Fact]
    public async Task Garbage_bytes_answer_fatal_PROTOCOL_and_close_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.OpenAsync(Server, alice.Access, "alice/garbage");
        await ws.SendBytesAsync([0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true);
        await ws.ExpectClosedAsync(PolicyViolation);
    }

    [Fact]
    public async Task No_hello_within_5_seconds_answers_fatal_PROTOCOL_and_closes_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.OpenAsync(Server, alice.Access, "alice/silent");
        var elapsed = Stopwatch.StartNew();
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true, TimeSpan.FromSeconds(8));
        Assert.InRange(elapsed.Elapsed.TotalSeconds, 4, 7);
        await ws.ExpectClosedAsync(PolicyViolation);
    }

    [Fact]
    public async Task A_duplicate_hello_answers_fatal_PROTOCOL_and_closes_1008()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var ws = await WsClient.ConnectAsync(Server, alice, "alice/dup");
        await ws.SendAsync(Frames.Hello());
        await ws.ExpectErrorAsync(ErrorCode.Protocol, fatal: true);
        await ws.ExpectClosedAsync(PolicyViolation);
    }

    [Fact]
    public async Task A_second_session_replaces_the_first_and_nobody_else_notices()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, a1) = (party.Account(0), party.Client(0));
        var (carol, c1) = (party.Account(1), party.Client(1));

        await using var a2 = await WsClient.OpenAsync(Server, alice.Access, "alice/new");
        await a2.SendAsync(Frames.Hello());
        await a1.ExpectErrorAsync(ErrorCode.SessionReplaced, fatal: true);
        Assert.Equal(PolicyViolation, await a1.ExpectClosedAsync(PolicyViolation));

        var session = await a2.ReadHelloAsync(alice);
        var ids = session.GeneralState.Members.Select(member => member.UserId).ToArray();
        Assert.Single(ids, id => id == alice.UserId);
        Assert.Contains(carol.UserId, ids);
        await c1.QuietAsync();

        var text = $"back again {Frames.NowMs()}";
        await a2.SendAsync(Frames.Send(text));
        var echo = (await a2.ExpectAsync(Kind.Message)).Message;
        var mirror = (await c1.ExpectAsync(Kind.Message)).Message;
        Assert.Equal(echo.Id, mirror.Id);
        Assert.Equal(text, mirror.Text);
    }

    [Fact]
    public async Task A_disconnect_reaches_the_remaining_members_as_MemberLeft()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        await party.Client(1).CloseAsync();
        await party.Client(0).ExpectMemberLeftAsync(Session.General, party.Account(1));
    }
}

using System.Net;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class DmTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task OpenDm_opens_a_room_only_its_two_members_ever_see()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var dmId = await party.OpenDmAsync(0, 1);
        Assert.Equal(Party.DmId(party.Account(0), party.Account(1)), dmId);
        await party.Client(2).QuietAsync();
    }

    [Fact]
    public async Task OpenDm_again_resyncs_the_caller_and_the_DM_with_oneself_or_leaving_it_answer_FORBIDDEN()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob");
        var (alice, a1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var dmId = await party.OpenDmAsync(0, 1);

        await b1.SendAsync(Frames.OpenDm(alice.UserId));
        var state = (await b1.ExpectAsync(Kind.RoomState)).RoomState;
        Assert.Equal(dmId, state.RoomId);
        await b1.QuietAsync();
        await a1.QuietAsync();

        await a1.SendAsync(Frames.OpenDm(alice.UserId));
        await a1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);
        await a1.SendAsync(Frames.Leave(dmId));
        await a1.ExpectErrorAsync(ErrorCode.Forbidden, fatal: false);
    }

    [Fact]
    public async Task DM_messages_and_history_stay_between_the_two_members()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (a1, b1, c1) = (party.Client(0), party.Client(1), party.Client(2));
        var (bob, carol) = (party.Account(1), party.Account(2));
        var dmId = await party.OpenDmAsync(0, 1);

        var secret = $"dm only {Names.Token()}";
        await a1.SendAsync(Frames.Send(secret, dmId));
        foreach (var client in new[] { a1, b1 })
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(dmId, message.RoomId);
            Assert.Equal(secret, message.Text);
        }

        await c1.QuietAsync();

        var outsider = await History.RawAsync(Server, carol.Access, room: dmId);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);

        var page = await History.PageAsync(Server, bob.Access, room: dmId);
        Assert.Equal(secret, page.Messages[^1].Text);
    }

    [Fact]
    public async Task A_disconnect_reaches_every_room_the_member_was_in()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "bob", "carol");
        var (a1, b1, c1) = (party.Client(0), party.Client(1), party.Client(2));
        var bob = party.Account(1);
        var dmId = await party.OpenDmAsync(0, 1);

        await b1.CloseAsync();
        var left = new[] { (await a1.ExpectAsync(Kind.MemberLeft)).MemberLeft, (await a1.ExpectAsync(Kind.MemberLeft)).MemberLeft };
        Assert.Equal(new[] { dmId, Session.General }.Order().ToArray(), left.Select(frame => frame.RoomId).Order().ToArray());
        Assert.All(left, frame => Assert.Equal(bob.UserId, frame.UserId));

        await c1.ExpectMemberLeftAsync(Session.General, bob);
        await c1.QuietAsync();
    }
}

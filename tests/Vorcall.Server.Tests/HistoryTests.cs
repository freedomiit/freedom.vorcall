using System.Globalization;
using System.Net;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class HistoryTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task History_pages_100_ascending_with_an_exclusive_before_and_clamped_limits()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var token = Names.Token();
        await using (var a1 = await WsClient.ConnectAsync(Server, alice))
        {
            for (var index = 0; index < 105; index++)
            {
                await a1.SendAsync(Frames.Send($"seed {token} {index}"));
                await a1.ExpectAsync(Kind.Message, TimeSpan.FromSeconds(10));
            }
        }

        var page = await History.PageAsync(Server, alice.Access);
        var ids = page.Messages.Select(message => message.Id).ToArray();
        Assert.Equal(100, ids.Length);
        Assert.Equal(ids.Order().ToArray(), ids);
        Assert.True(page.HasMore);
        Assert.All(page.Messages, message => Assert.Equal(Session.General, message.RoomId));
        Assert.Contains(page.Messages, message => message.AuthorId > 0);

        var older = await History.PageAsync(Server, alice.Access, before: ids[0].ToString(CultureInfo.InvariantCulture));
        Assert.NotEmpty(older.Messages);
        Assert.All(older.Messages, message => Assert.True(message.Id < ids[0]));

        var scoped = await History.PageAsync(Server, alice.Access, room: Session.General);
        Assert.Equal(ids, scoped.Messages.Select(message => message.Id).ToArray());

        Assert.Single((await History.PageAsync(Server, alice.Access, limit: "0")).Messages);
        Assert.Equal(100, (await History.PageAsync(Server, alice.Access, limit: "1000")).Messages.Count);
    }

    [Fact]
    public async Task History_refuses_unusable_query_values()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        await using var a1 = await WsClient.ConnectAsync(Server, alice);

        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, room: "bad!")).Status);
        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, before: "abc")).Status);
        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, before: "0")).Status);
        Assert.Equal(HttpStatusCode.BadRequest, (await History.RawAsync(Server, alice.Access, limit: "abc")).Status);
    }

    [Fact]
    public async Task History_of_a_room_not_joined_answers_403()
    {
        await using var party = await Party.ConnectAsync(Server, "alice", "carol");
        var (alice, carol) = (party.Account(0), party.Account(1));
        var (_, roomId) = await party.CreateRoomAsync();

        var outsider = await History.RawAsync(Server, carol.Access, room: roomId);
        Assert.Equal(HttpStatusCode.Forbidden, outsider.Status);
        Assert.Equal("not a member", outsider.Detail);

        await History.PageAsync(Server, alice.Access, room: roomId);
    }
}

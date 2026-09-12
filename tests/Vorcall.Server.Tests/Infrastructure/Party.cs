using Vorcall.Server.Protocol;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests.Infrastructure;

// Several accounts, connected in order, with the presence frames their arrivals caused already
// consumed: the starting point of most wire tests. Every account belongs to the server and sees
// every channel VIEW_CHANNEL resolves for, so there is nothing to join.
internal sealed class Party : IAsyncDisposable
{
    private readonly List<Account> _accounts;
    private readonly List<WsClient> _clients;
    private readonly bool _ownerFirst;

    // Every role but @everyone that exists on the server, which is what decides whether the next
    // CreateRole renumbers the others: read from the first socket's snapshot, which carries every
    // role, and kept up to date by CreateRoleAsync. Lazy because the sockets connect after the
    // constructor.
    private HashSet<long>? _roles;

    private Party(List<Account> accounts, List<WsClient> clients, bool ownerFirst)
    {
        _accounts = accounts;
        _clients = clients;
        _ownerFirst = ownerFirst;
    }

    public IReadOnlyList<Account> Accounts => _accounts;

    public IReadOnlyList<WsClient> Clients => _clients;

    // The general channel's id, as the first socket's snapshot named it.
    public long GeneralId => _clients[0].Session.GeneralId;

    // The voice channel the seed puts beside general.
    public long GeneralVoiceId => _clients[0].Session.GeneralVoiceId;

    // The undeletable default role every member holds implicitly, which is the target of most
    // overrides.
    public long EveryoneId => _clients[0].Session.Everyone.Id;

    public Account Account(int index) => _accounts[index];

    public WsClient Client(int index) => _clients[index];

    // Fresh accounts under unique names, none of them privileged: @everyone grants VIEW_CHANNEL,
    // SEND_MESSAGES, ATTACH_FILES, ADD_REACTIONS, CONNECT, SPEAK, SHARE_SCREEN and
    // CHANGE_NICKNAME, and nothing else.
    public static Task<Party> ConnectAsync(VorcallFactory factory, params string[] prefixes)
        => ConnectAsync(factory, null, prefixes);

    // The same, with the server's owner as member 0: the owner bypasses every permission check, so
    // this is the party whose first member may manage channels, roles, overrides and members. The
    // owner account is the host's own, shared by every test of the collection, rather than a fresh
    // registration — ownership is a server row, and only a boot reads it. One such party may be
    // alive at a time: a second would replace the owner's socket, and the first would see a fatal
    // SESSION_REPLACED.
    public static async Task<Party> WithOwnerAsync(VorcallFactory factory, ServerFixture fixture, params string[] prefixes)
        => await ConnectAsync(factory, await fixture.OwnerAsync(factory), prefixes);

    private static async Task<Party> ConnectAsync(VorcallFactory factory, Account? first, string[] prefixes)
    {
        var accounts = new List<Account>(prefixes.Length + 1);
        if (first is not null)
        {
            accounts.Add(first);
        }

        foreach (var prefix in prefixes)
        {
            accounts.Add(await Infrastructure.Accounts.RegisterAsync(factory, prefix));
        }

        var clients = new List<WsClient>(accounts.Count);
        var party = new Party(accounts, clients, first is not null);
        try
        {
            foreach (var account in accounts)
            {
                var client = await WsClient.ConnectAsync(factory, account);

                // A connection that is not a replacement is announced to every other online member.
                foreach (var earlier in clients)
                {
                    await earlier.ExpectMemberUpdatedAsync(account, online: true);
                }

                clients.Add(client);
            }
        }
        catch
        {
            await party.DisposeAsync();
            throw;
        }

        return party;
    }

    // Swaps one member's socket for a reconnected one; the old socket is already closed.
    public async Task ReconnectAsync(VorcallFactory factory, int index)
    {
        var replacement = await WsClient.ReconnectAsync(factory, _accounts[index], _clients[index]);
        await _clients[index].DisposeAsync();
        _clients[index] = replacement;
    }

    // A channel created by the first member, which needs MANAGE_CHANNELS and so a party built with
    // WithOwnerAsync. Consumes what the creation actually causes: one ChannelUpserted on every
    // socket that may view it, which with no override on the channel is all of them. The id is the
    // server's, so it is read from the frames rather than predicted.
    public async Task<(string Name, long Id)> CreateChannelAsync(ChannelKind kind = ChannelKind.Text, long categoryId = 0)
    {
        var name = $"chat {Names.Token()}";
        await _clients[0].SendAsync(Frames.CreateChannel(kind, name, categoryId: categoryId));

        long id = 0;
        foreach (var client in _clients)
        {
            var channel = (await client.ExpectAsync(Kind.ChannelUpserted)).ChannelUpserted.Channel;
            Assert.Equal(name, channel.Name);
            Assert.Equal(kind, channel.Kind);
            Assert.Equal(categoryId, channel.CategoryId);
            Assert.Empty(channel.Overrides);
            Assert.True(channel.Id != 0, $"{client.Label}: the new channel has no id");
            Assert.True(id == 0 || id == channel.Id, $"{client.Label}: channel id {channel.Id} differs from {id}");
            id = channel.Id;
        }

        return (name, id);
    }

    // Opens the DM between two members from the caller's socket and consumes what a first open
    // sends to both parties: ChannelUpserted for the DM, then its VoiceState.
    public async Task<long> OpenDmAsync(int caller, int other)
    {
        var members = new[] { _accounts[caller].UserId, _accounts[other].UserId }.Order().ToArray();
        await _clients[caller].SendAsync(Frames.OpenDm(_accounts[other].UserId));

        long id = 0;
        foreach (var index in new[] { caller, other })
        {
            var client = _clients[index];
            var channel = (await client.ExpectAsync(Kind.ChannelUpserted)).ChannelUpserted.Channel;
            Assert.Equal(ChannelKind.Dm, channel.Kind);
            Assert.Equal(string.Empty, channel.Name);
            Assert.Equal(members, channel.DmMemberIds.Order().ToArray());
            Assert.True(channel.Id != 0, $"{client.Label}: the DM channel has no id");
            Assert.True(id == 0 || id == channel.Id, $"{client.Label}: DM channel id {channel.Id} differs from {id}");
            id = channel.Id;

            var state = (await client.ExpectAsync(Kind.VoiceState)).VoiceState;
            Assert.Equal(id, state.ChannelId);
            Assert.Empty(state.Members);
        }

        return id;
    }

    // CreateRole by the first member, which needs MANAGE_ROLES and so the owner party. Every role
    // frame goes to every online member, so this consumes one RoleUpserted per socket, followed by
    // the RoleOrder that a create sends when it renumbered the roles that were already there: the
    // insert lands at position 1 (the owner holds no role, so its highest position is 0), which
    // renumbers whenever any role but @everyone exists — and they accumulate over a run, so this is
    // the usual case, not the exception. Returns the new role's id.
    public async Task<long> CreateRoleAsync(ulong permissions, bool hoist = false)
    {
        var name = $"role {Names.Token()}";
        var renumbers = Roles.Count > 0;
        await _clients[0].SendAsync(Frames.CreateRole(name, permissions, hoist: hoist));

        long id = 0;
        foreach (var client in _clients)
        {
            var role = (await client.ExpectAsync(Kind.RoleUpserted)).RoleUpserted.Role;
            Assert.Equal(name, role.Name);
            Assert.Equal(permissions, role.Permissions);
            Assert.Equal(hoist, role.Hoist);
            Assert.False(role.Everyone, $"{client.Label}: a created role must not be the everyone role");
            Assert.True(role.Id != 0, $"{client.Label}: the new role has no id");
            Assert.True(id == 0 || id == role.Id, $"{client.Label}: role id {role.Id} differs from {id}");
            id = role.Id;

            if (renumbers)
            {
                // The list is bottom first and the insert landed at position 1, so the new role is
                // the bottom of it.
                var order = (await client.ExpectAsync(Kind.RoleOrder)).RoleOrder;
                Assert.True(
                    order.Ids.Count > 0 && order.Ids[0] == id,
                    $"{client.Label}: the new role is not the bottom of [{string.Join(", ", order.Ids)}]");
            }
        }

        Roles.Add(id);
        return id;
    }

    // SetMemberRoles by the first member, replacing that member's roles outright. Consumes the one
    // MemberUpdated every online member receives, the target included.
    //
    // The target must not be in a voice session and the roles must not change what it may view: the
    // server re-resolves the target against every channel right after the broadcast, and either of
    // those sends it further frames of its own. Never the owner: its roles outlive the test, and the
    // whole collection shares the account.
    public async Task SetMemberRolesAsync(int index, params long[] roleIds)
    {
        Assert.False(
            _ownerFirst && index == 0,
            "the owner's roles are shared by the whole collection; give the role to another member");

        var target = _accounts[index];
        await _clients[0].SendAsync(Frames.SetMemberRoles(target.UserId, roleIds));
        foreach (var client in _clients)
        {
            var member = await client.ExpectMemberUpdatedAsync(target);
            Assert.Equal(roleIds.Order().ToArray(), member.RoleIds.Order().ToArray());
        }
    }

    // CreateRole then SetMemberRoles: the short way to a member holding exactly the bits under test,
    // which is a more honest privileged actor than the owner for anything but an owner-only rule.
    // Consumes both fixtures' frames in that order and returns the role's id.
    public async Task<long> GrantRoleAsync(int index, ulong permissions, bool hoist = false)
    {
        var roleId = await CreateRoleAsync(permissions, hoist);
        await SetMemberRolesAsync(index, roleId);
        return roleId;
    }

    // SetOverride for a role, by the first member, which needs MANAGE_CHANNELS on the channel.
    public Task<Channel> SetRoleOverrideAsync(long channelId, long roleId, ulong allow = 0, ulong deny = 0)
        => SetOverrideAsync(channelId, Frames.RoleOverride(roleId, allow, deny), allow, deny);

    // The same for a single member's own override.
    public Task<Channel> SetMemberOverrideAsync(long channelId, int index, ulong allow = 0, ulong deny = 0)
        => SetOverrideAsync(channelId, Frames.MemberOverride(_accounts[index].UserId, allow, deny), allow, deny);

    // Every socket receives one ChatMessage with that text; returns the id they all carry.
    public async Task<long> ExpectMessageEverywhereAsync(string text, long channelId)
    {
        long id = 0;
        foreach (var client in _clients)
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(text, message.Text);
            Assert.Equal(channelId, message.ChannelId);
            Assert.True(id == 0 || id == message.Id, $"{client.Label}: message id {message.Id} differs from {id}");
            id = message.Id;
        }

        return id;
    }

    // One frame of that kind on every socket, in party order.
    public async Task<ServerFrame[]> ExpectEverywhereAsync(Kind kind)
    {
        var frames = new ServerFrame[_clients.Count];
        for (var i = 0; i < _clients.Count; i++)
        {
            frames[i] = await _clients[i].ExpectAsync(kind);
        }

        return frames;
    }

    private HashSet<long> Roles => _roles ??=
        [.. _clients[0].Session.Roles.Values.Where(role => !role.Everyone).Select(role => role.Id)];

    // One ChannelUpserted to every member who may view the channel, which the sweep that runs first
    // has already measured: a member whose sight of the channel the override changed hears about that
    // before the upsert. VIEW_CHANNEL is refused here for that reason — granting or denying it makes
    // the frames depend on who gained or lost the channel, a member that gained it receiving the
    // upsert twice, once from the sweep and once from the broadcast, and a member that lost it
    // receiving ChannelDeleted instead. That test drives Frames.SetOverride itself.
    //
    // allow and deny carry channel-scoped bits only, as the wire has it, and allow == deny == 0
    // deletes the override.
    private async Task<Channel> SetOverrideAsync(long channelId, Override over, ulong allow, ulong deny)
    {
        Assert.True(
            ((allow | deny) & (ulong)Permissions.Perm.ViewChannel) == 0,
            "an override of VIEW_CHANNEL changes who may see the channel; send Frames.SetOverride yourself");

        await _clients[0].SendAsync(Frames.SetOverride(channelId, over));

        Channel? upserted = null;
        foreach (var client in _clients)
        {
            var channel = await client.ExpectChannelUpsertedAsync(channelId);
            var stored = channel.Overrides
                .Where(row => row.RoleId == over.RoleId && row.UserId == over.UserId)
                .ToArray();
            if ((allow | deny) == 0)
            {
                Assert.Empty(stored);
            }
            else
            {
                Assert.Single(stored);
                Assert.Equal(allow, stored[0].Allow);
                Assert.Equal(deny, stored[0].Deny);
            }

            upserted ??= channel;
        }

        return upserted!;
    }

    public async ValueTask DisposeAsync()
    {
        foreach (var client in _clients)
        {
            await client.DisposeAsync();
        }
    }
}

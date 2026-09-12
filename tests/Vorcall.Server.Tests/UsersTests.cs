using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class UsersTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Users_lists_every_account_ordered_case_insensitively()
    {
        var alice = await Accounts.RegisterAsync(Server, "alice");
        var bob = await Accounts.RegisterAsync(Server, "bob");
        var carol = await Accounts.RegisterAsync(Server, "carol");

        var listing = await Accounts.ListUsersAsync(Server, alice.Access);
        var names = listing.Users.Select(user => user.Username).ToArray();
        Assert.Equal(names.OrderBy(name => name, StringComparer.OrdinalIgnoreCase).ToArray(), names);

        var byName = listing.Users.ToDictionary(user => user.Username, user => user.UserId);
        foreach (var account in new[] { alice, bob, carol })
        {
            Assert.Equal(account.UserId, byName[account.Username]);
        }

        var mine = new HashSet<string>(StringComparer.Ordinal) { alice.Username, bob.Username, carol.Username };
        Assert.Equal(new[] { alice.Username, bob.Username, carol.Username }, names.Where(mine.Contains).ToArray());
    }
}

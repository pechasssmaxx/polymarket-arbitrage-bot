Dropped my polymarket arbitrage bot

it checks every market, spots the profit, automatically grabs the needed options, and hits convert

the alpha is that multi outcome markets have a negriskadapter contract, you will see it on the cup badge markets, and it allows you to flip your NO tokens back to usdc and grab free YES tokens on top

numbers example

5 option market, we buy NO on 3 picks k=3
> NO #1 60¢  
> NO #2 55¢  
> NO #3 70¢
one share costs 1.85$

we give up 3 NO and get back 2 usdc plus 3 YES

so how does it actually work?

the bot connects to the polymarket websocket and listens to all orderbook updates in real time, every 5 minutes it refreshes the list of active markets

every time the orderbook moves the bot runs quickcheck, a 4 microsecond speed filter, it cuts off 99% of the markets where there is no edge at all

if quickcheck passes it calculates the real price based on orderbook depth, not just the best ask, but how much it actually costs to buy n shares, because there might be 1 share sitting at 50 cents and then a wall at 95

it loops through all outcome combinations, finds the optimal K for how many NO to buy and the best position size, then it runs a binary search on the size for each K

if you are interested i will drop trader examples and the bot in the comments

but always remember the risks

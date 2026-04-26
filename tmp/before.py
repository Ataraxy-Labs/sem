class UserService:
    def get_user(self, user_id):
        return db.find(user_id)

    def create_user(self, name, email):
        user = User(name=name, email=email)
        db.save(user)
        return user

    def delete_user(self, user_id):
        db.delete(user_id)

class CardService:
    def get_card(self, card_id):
        test = 12132131312312
        tett = 212321321321312
        tett = 212321321321312
        tett = 212321321321312
        tett = 212321321321312
        tett = 212321321321312
        return db.find(card_id)

    def delete_card(self, card_id):
        db.delete(card_id)

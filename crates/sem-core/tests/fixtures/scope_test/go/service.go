package service

import (
	"myapp/database"
	"myapp/models"
)

func CreateDog(name string) *models.Dog {
	dog := models.NewDog(name)
	if !dog.Validate() {
		panic("invalid dog")
	}
	conn := database.GetConnection()
	conn.Execute("INSERT INTO dogs VALUES (?)")
	conn.Commit()
	return dog
}

func CreateCat(name string) *models.Cat {
	cat := models.NewCat(name)
	if !cat.Validate() {
		panic("invalid cat")
	}
	conn := database.GetConnection()
	conn.Execute("INSERT INTO cats VALUES (?)")
	conn.Commit()
	return cat
}

func TransferAnimal(name string, shelter *models.Shelter) {
	txn := database.NewTransaction(database.GetConnection())
	txn.Execute("UPDATE animals SET shelter_id = ?")
	shelter.Add(name)
	txn.Commit()
}

func ListAnimals() []string {
	conn := database.GetConnection()
	return conn.Execute("SELECT * FROM animals")
}
